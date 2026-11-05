//! The MCP server on stdio: `merge_documents` merges files as the CLI does and answers with its
//! `--json` object.

use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use doc_merge::error::Error;
use doc_merge::paths::{self, Target};
use doc_merge::{Options, Report, STOP, StyleMode, TIME_LIMIT, guarded};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject};
use rmcp::service::ServerInitializeError;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::generate::SchemaSettings;
use schemars::transform::RecursiveTransform;
use schemars::{JsonSchema, Schema};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MergeArgs {
    /// Files, folders or patterns, at least 2 files after expansion; relative paths resolve
    /// against the server's working directory
    #[schemars(length(min = 1, max = 500))]
    files: Vec<String>,
    /// File or folder; default merged.<ext> beside the first file
    output: Option<String>,
    /// Word: keep each document's look, or take the first document's styles
    #[serde(default)]
    #[schemars(with = "String", extend("enum" = ["keep", "first"], "default" = "keep"))]
    styles: StyleMode,
    /// Replace an existing output file
    #[serde(default)]
    overwrite: bool,
}

#[derive(Clone)]
struct Server;

#[tool_router]
impl Server {
    #[tool(
        description = "Merge two or more Word (.docx/.dotx), PowerPoint (.pptx/.potx/.ppsx) or Excel (.xlsx/.xltx) files into one file of the same kind, in the order given, and save it. PowerPoint keeps each deck's masters, layouts and theme (identical ones are shared), notes, charts, comments, sections and slide links; slides of another size are scaled to fit. Excel copies every sheet with its formats, tables, charts, names and comments; clashing sheet, table and name references are renamed and formulas rewritten. Word starts each document on a new page with its own page setup, headers and footers; lists restart as in the source; clashing styles keep each document's look (styles \"keep\") or take the first document's (styles \"first\"). Folders and * patterns expand in natural order. Refuses macros, password-protected files, and Excel pivot tables, slicers and data models in files after the first, naming the reason. Never overwrites unless overwrite is true. Returns the output path, which slides, sheets or sections came from each file, every renamed item and warnings; on failure the result has isError true and one text block holding {\"error\":{\"code\",\"message\",\"hint\",\"input\"}}.",
        input_schema = input_schema(),
        output_schema = schema::<Report>(SchemaSettings::draft2020_12().for_serialize()),
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn merge_documents(&self, args: JsonObject) -> CallToolResult {
        answer(parse(args).and_then(|a| merge(a, Instant::now() + TIME_LIMIT)))
    }
}

#[tool_handler(
    name = "doc-merge",
    version = "0.1.0",
    instructions = "Merge several Word (.docx), PowerPoint (.pptx) or Excel (.xlsx) files into one file of the same kind, keeping layouts and slide masters, charts, speaker notes, styles, numbering, headers and footers. Files are merged in the order given; the first file's settings are the base. Pass absolute paths."
)]
impl ServerHandler for Server {}

/// The arguments, refusing names the tool doesn't define.
fn parse(args: JsonObject) -> Result<MergeArgs, Error> {
    serde_json::from_value(Value::Object(args))
        .map_err(|e| Error::usage(format!("Can't read the arguments: {e}.")))
}

/// The report as the result, or the error object as an error result. Errors carry no structured
/// content, so clients don't check them against the output schema.
fn answer(result: Result<Value, Error>) -> CallToolResult {
    result.map_or_else(
        |e| CallToolResult::error(vec![ContentBlock::text(e.to_json())]),
        CallToolResult::structured,
    )
}

/// Merges as the CLI does, answering with its `--json` object.
fn merge(args: MergeArgs, deadline: Instant) -> Result<Value, Error> {
    let files: Vec<PathBuf> = args.files.iter().map(PathBuf::from).collect();
    let files = paths::expand(&files)?;
    let target = Target {
        output: args.output.map(PathBuf::from),
        overwrite: args.overwrite,
    };
    let options = Options {
        styles: args.styles,
    };
    let report =
        crate::merging(|| guarded(|| doc_merge::merge(&files, &target, &options, Some(deadline))));
    // A Ctrl+C has removed the temp file by now, and ends the server.
    if STOP.load(Ordering::SeqCst) {
        process::exit(130);
    }
    Ok(serde_json::to_value(report?).expect("results are plain data"))
}

/// The tool's arguments, closed to names it doesn't define.
fn input_schema() -> Arc<JsonObject> {
    let mut schema = schema::<MergeArgs>(SchemaSettings::draft2020_12());
    Arc::make_mut(&mut schema).insert("additionalProperties".into(), false.into());
    schema
}

/// A schema with an object at its root, as 2025-11-25 clients require, and without the Rust
/// number formats schemars adds (uint32, int64), which JSON Schema doesn't define.
fn schema<T: JsonSchema>(settings: SchemaSettings) -> Arc<JsonObject> {
    let no_format = |s: &mut Schema| drop(s.remove("format"));
    let schema = settings
        .with_transform(RecursiveTransform(no_format))
        .into_generator()
        .into_root_schema_for::<T>();
    let Value::Object(mut schema) = schema.to_value() else {
        unreachable!("a root schema is an object")
    };
    schema.remove("title");
    schema.remove("description");
    schema.insert("type".into(), "object".into());
    Arc::new(schema)
}

/// Serves until the client closes stdin. Calls run one at a time on this thread; each merge runs
/// on its own worker.
pub fn serve() -> Result<(), Error> {
    let failed = |e: &dyn std::error::Error| Error::internal("the MCP server stopped").because(e);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| failed(&e))?;
    runtime.block_on(async {
        match Server.serve(rmcp::transport::stdio()).await {
            Ok(service) => service.waiting().await.map(drop).map_err(|e| failed(&e)),
            // stdin closed before the handshake ended: the client left, which is no failure.
            Err(ServerInitializeError::ConnectionClosed(_)) => Ok(()),
            // stdout closed before the answer went out: the client left too.
            Err(ServerInitializeError::TransportError { error, .. })
                if error
                    .error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) =>
            {
                Ok(())
            }
            Err(e) => Err(failed(&e)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(result: CallToolResult) -> Value {
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
        let [block] = &result.content[..] else {
            panic!("{result:?}")
        };
        let text = block.as_text().unwrap().text.as_str();
        serde_json::from_str::<Value>(text).unwrap()["error"].clone()
    }

    /// `a.docx` twice, from the committed fixtures.
    fn two_files() -> MergeArgs {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/a.docx").to_string();
        MergeArgs {
            files: vec![file.clone(), file],
            output: Some("never-written.docx".into()),
            styles: StyleMode::Keep,
            overwrite: false,
        }
    }

    #[test]
    fn a_merge_past_its_deadline_answers_timeout() {
        let e = error(answer(merge(two_files(), Instant::now())));
        assert_eq!(e["code"], "timeout");
        assert_eq!(e["message"], "Stopped after 600 s.");
        assert_eq!(
            e["hint"],
            "Merge fewer files at once, or use the command line."
        );
    }

    #[test]
    fn a_panic_answers_internal_and_the_next_call_still_works() {
        let e = guarded(|| -> Result<Value, Error> { panic!("boom") });
        let e = error(answer(e));
        assert_eq!(e["code"], "internal");
        assert_eq!(e["message"], "Something went wrong inside doc-merge: boom.");
        let next = answer(merge(two_files(), Instant::now()));
        assert_eq!(error(next)["code"], "timeout");
    }
}
