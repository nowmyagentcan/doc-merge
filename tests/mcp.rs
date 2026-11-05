//! Talks to `doc-merge mcp` over stdio the way MCP clients do.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_doc-merge");

const INSTRUCTIONS: &str = "Merge several Word (.docx), PowerPoint (.pptx) or Excel (.xlsx) files into one file of the same kind, keeping layouts and slide masters, charts, speaker notes, styles, numbering, headers and footers. Files are merged in the order given; the first file's settings are the base. Pass absolute paths.";

const DESCRIPTION: &str = "Merge two or more Word (.docx/.dotx), PowerPoint (.pptx/.potx/.ppsx) or Excel (.xlsx/.xltx) files into one file of the same kind, in the order given, and save it. PowerPoint keeps each deck's masters, layouts and theme (identical ones are shared), notes, charts, comments, sections and slide links; slides of another size are scaled to fit. Excel copies every sheet with its formats, tables, charts, names and comments; clashing sheet, table and name references are renamed and formulas rewritten. Word starts each document on a new page with its own page setup, headers and footers; lists restart as in the source; clashing styles keep each document's look (styles \"keep\") or take the first document's (styles \"first\"). Folders and * patterns expand in natural order. Refuses macros, password-protected files, and Excel pivot tables, slicers and data models in files after the first, naming the reason. Never overwrites unless overwrite is true. Returns the output path, which slides, sheets or sections came from each file, every renamed item and warnings; on failure the result has isError true and one text block holding {\"error\":{\"code\",\"message\",\"hint\",\"input\"}}.";

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1.0.0"}}}"#;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A folder holding copies of the named fixtures.
fn folder(test: &str, fixtures: &[&str]) -> PathBuf {
    let dir = temp_dir(&format!("mcp-{test}"));
    for name in fixtures {
        fs::copy(fixture(name), dir.join(name)).unwrap();
    }
    dir
}

fn command(dir: &Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .arg("mcp")
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Sends each message from `dir`, closes stdin, and returns the responses by id and the exit
/// code. The server writes nothing to stderr.
fn session(dir: &Path, messages: &[Value]) -> (Vec<Value>, i32) {
    let mut child = command(dir).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{INITIALIZE}").unwrap();
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    writeln!(stdin, "{initialized}").unwrap();
    for m in messages {
        writeln!(stdin, "{m}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let mut responses: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    responses.sort_by_key(|r| r["id"].as_u64());
    (responses, out.status.code().unwrap())
}

fn call(id: u64, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "merge_documents", "arguments": arguments}})
}

/// The answer to one tool call.
fn result(dir: &Path, arguments: Value) -> Value {
    let (responses, code) = session(dir, &[call(2, arguments)]);
    assert_eq!(code, 0);
    responses[1]["result"].clone()
}

/// A result holding the report as structured content and as its one text block.
fn report(result: &Value) -> &Value {
    assert_eq!(result["isError"], false, "{result}");
    let blocks = result["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1, "{result}");
    let parsed: Value = serde_json::from_str(blocks[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed, result["structuredContent"]);
    &result["structuredContent"]
}

/// An error result: one text block holding the error object and no structured content.
fn refused(result: &Value) -> Value {
    assert_eq!(result["isError"], true, "{result}");
    assert!(result.get("structuredContent").is_none(), "{result}");
    let blocks = result["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    let e: Value = serde_json::from_str(blocks[0]["text"].as_str().unwrap()).unwrap();
    e["error"].clone()
}

/// The same report without what differs from run to run.
fn timeless(report: &Value) -> Value {
    let mut report = report.clone();
    report.as_object_mut().unwrap().remove("elapsed_ms");
    report
}

#[test]
fn initializes_and_lists_the_one_tool_as_documented() {
    let dir = folder("list", &[]);
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let (responses, code) = session(&dir, &[list]);
    assert_eq!(code, 0);
    let init = &responses[0]["result"];
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert_eq!(
        init["serverInfo"],
        json!({"name": "doc-merge", "version": "0.1.0"})
    );
    assert_eq!(init["instructions"], INSTRUCTIONS);
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    let [tool] = &tools[..] else {
        panic!("{tools:?}")
    };
    assert_eq!(tool["name"], "merge_documents");
    assert_eq!(tool["description"], DESCRIPTION);
    assert_eq!(
        tool["annotations"],
        json!({"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false})
    );
    let input = &tool["inputSchema"];
    assert_eq!(input["additionalProperties"], false);
    assert_eq!(input["required"], json!(["files"]));
    assert!(!input.to_string().contains("\"format\""), "{input}");
    let properties = input["properties"].as_object().unwrap();
    let names: Vec<_> = properties.keys().map(String::as_str).collect();
    assert_eq!(names, ["files", "output", "styles", "overwrite"]);
    let files = &properties["files"];
    assert_eq!(
        (&files["minItems"], &files["maxItems"]),
        (&json!(1), &json!(500))
    );
    assert_eq!(properties["styles"]["enum"], json!(["keep", "first"]));
    assert_eq!(properties["styles"]["default"], "keep");
    assert_eq!(properties["overwrite"]["default"], false);
    assert_eq!(
        tool["outputSchema"]["required"],
        json!([
            "output",
            "inputs",
            "renames",
            "renames_omitted",
            "warnings",
            "elapsed_ms"
        ])
    );
    assert!(tool["outputSchema"]["properties"].get("steps").is_none());
}

#[test]
fn the_2026_lifecycle_discovers_the_server_and_calls_the_tool() {
    let dir = folder("discover", &["a.docx", "b.docx"]);
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0.0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let request = |id: u64, method: &str, params: Value| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let merge = json!({"_meta": meta, "name": "merge_documents",
        "arguments": {"files": ["a.docx", "b.docx"]}});
    let mut child = command(&dir).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        request(1, "server/discover", json!({"_meta": meta}))
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        request(2, "tools/list", json!({"_meta": meta}))
    )
    .unwrap();
    writeln!(stdin, "{}", request(3, "tools/call", merge)).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let mut answers: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    answers.sort_by_key(|r| r["id"].as_u64());
    let found = &answers[0]["result"];
    let versions = found["supportedVersions"].as_array().unwrap();
    assert!(versions.contains(&json!("2026-07-28")), "{found}");
    assert!(versions.contains(&json!("2025-11-25")), "{found}");
    assert_eq!(
        found["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "doc-merge"
    );
    assert_eq!(found["instructions"], INSTRUCTIONS);
    let tools = answers[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    report(&answers[2]["result"]);
    assert!(dir.join("merged.docx").exists());
}

#[test]
fn a_merge_equals_the_cli_for_each_family() {
    for (name, files) in [
        ("word", ["a.docx", "b.docx"]),
        ("excel", ["a.xlsx", "b.xlsx"]),
        ("powerpoint", ["a.pptx", "c.pptx"]),
    ] {
        let dir = folder(name, &files);
        let ext = files[0].rsplit('.').next().unwrap();
        let via_mcp = format!("via-mcp.{ext}");
        let via_cli = format!("via-cli.{ext}");
        let answer = result(&dir, json!({"files": files, "output": via_mcp}));
        let got = report(&answer);
        let out = Command::new(BIN)
            .args(files)
            .args(["-o", &via_cli, "--json"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let cli: Value = serde_json::from_slice(&out.stdout).unwrap();
        // The file's own name is all that may differ.
        let same = timeless(got).to_string().replace("via-mcp", "via-cli");
        assert_eq!(same, timeless(&cli).to_string());
        assert_eq!(
            fs::read(dir.join(&via_mcp)).unwrap(),
            fs::read(dir.join(&via_cli)).unwrap()
        );
        let output = PathBuf::from(got["output"]["path"].as_str().unwrap());
        assert!(output.is_absolute(), "{output:?}");
    }
}

#[test]
fn styles_outputs_and_folders_work_as_on_the_command_line() {
    let dir = folder("options", &["a.docx", "b.docx"]);
    let first = result(&dir, json!({"files": [&dir], "styles": "first"}));
    let keep = result(
        &dir,
        json!({"files": ["a.docx", "b.docx"], "styles": "keep"}),
    );
    let name = |r: &Value| report(r)["output"]["path"].as_str().unwrap().to_string();
    assert!(name(&first).ends_with("merged.docx"), "{first}");
    assert!(name(&keep).ends_with("merged-2.docx"), "{keep}");
    let styles = |r: &Value| {
        let renames = report(r)["renames"].as_array().unwrap().iter();
        renames.filter(|r| r["kind"] == "style").count()
    };
    assert_eq!(styles(&first), 0);
    assert!(styles(&keep) > 0);

    let args = json!({"files": ["a.docx", "b.docx"], "output": "merged.docx"});
    assert_eq!(refused(&result(&dir, args))["code"], "output_exists");
    let args = json!({"files": ["a.docx", "b.docx"], "output": "merged.docx", "overwrite": true});
    report(&result(&dir, args));
}

#[test]
fn invalid_or_unknown_arguments_are_refused_without_structured_content() {
    let dir = folder("bad", &["a.docx", "a.pptx"]);
    let many = vec!["a.docx"; 501];
    for (args, code, message) in [
        (json!({}), "usage", "missing field `files`"),
        (json!({"files": "a.docx"}), "usage", "invalid type"),
        (
            json!({"files": ["a.docx", "a.docx"], "style": "keep"}),
            "usage",
            "unknown field `style`",
        ),
        (
            json!({"files": ["a.docx", "a.docx"], "styles": "last"}),
            "usage",
            "unknown variant `last`",
        ),
        (json!({"files": []}), "no_input", "No files given."),
        (
            json!({"files": ["a.docx"]}),
            "need_two_files",
            "at least two files",
        ),
        (
            json!({"files": many}),
            "limit_exceeded",
            "The number of files",
        ),
        (
            json!({"files": ["a.docx", "missing.docx"]}),
            "file_not_found",
            "missing.docx",
        ),
        (
            json!({"files": ["a.docx", "a.pptx"]}),
            "mixed_formats",
            "1 Word document and 1 PowerPoint deck",
        ),
        (
            json!({"files": ["a.docx", "a.docx"], "output": "x.txt"}),
            "bad_extension",
            "name it x.docx",
        ),
        (
            json!({"files": ["a.docx", "a.docx"], "output": "a.docx", "overwrite": true}),
            "output_is_input",
            "a.docx",
        ),
    ] {
        let e = refused(&result(&dir, args.clone()));
        assert_eq!(e["code"], code, "{args} {e}");
        let got = e["message"].as_str().unwrap();
        assert!(got.contains(message), "{got}");
    }
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
}

/// A live session, one call at a time.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(dir: &Path) -> Session {
        let mut child = command(dir).spawn().unwrap();
        let mut session = Session {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        writeln!(session.stdin, "{INITIALIZE}").unwrap();
        session.line();
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        writeln!(session.stdin, "{initialized}").unwrap();
        session
    }

    fn line(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "the server has gone");
        serde_json::from_str(&line).unwrap()
    }

    fn call(&mut self, id: u64, arguments: Value) -> Value {
        writeln!(self.stdin, "{}", call(id, arguments)).unwrap();
        let answer = self.line();
        assert_eq!(answer["id"], id, "{answer}");
        answer["result"].clone()
    }
}

#[test]
fn a_bad_file_leaves_the_server_answering_the_next_call() {
    let dir = folder("survives", &["a.docx", "b.docx"]);
    let mut session = Session::start(&dir);
    let mut id = 2;
    for (file, bytes, code) in hostile() {
        let path = write(&dir, file, &bytes);
        let args = json!({"files": ["a.docx", path], "output": "bad.docx"});
        let answer = session.call(id, args);
        assert_eq!(refused(&answer)["code"], code, "{file}");
        id += 1;
    }
    let args = json!({"files": ["a.docx", "b.docx"], "output": "good.docx"});
    report(&session.call(id, args));
    assert!(dir.join("good.docx").exists());
    assert!(!dir.join("bad.docx").exists());
    drop(session.stdin);
    assert_eq!(session.child.wait().unwrap().code(), Some(0));
}

#[test]
fn exits_quietly_when_stdin_closes() {
    let dir = folder("stdin", &[]);
    let mut child = command(&dir).stdin(Stdio::null()).spawn().unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        (out.status.code(), out.stdout.len(), out.stderr.len()),
        (Some(0), 0, 0)
    );
}

#[test]
fn exits_quietly_when_stdout_closes() {
    // The read end closes before the spawn: dropped after it, a test spawning beside this one
    // could inherit a copy and keep the pipe open.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let dir = folder("stdout", &[]);
    let mut child = command(&dir).stdout(writer).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let _ = writeln!(stdin, "{INITIALIZE}");
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "still running");
        thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        (status.code(), String::from_utf8_lossy(&out.stderr).as_ref()),
        (Some(0), "")
    );
}

#[test]
fn has_a_help_and_takes_no_arguments() {
    let run = |args: &[&str]| Command::new(BIN).args(args).output().unwrap();
    let out = run(&["mcp", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("Run the MCP server on stdio.\n"), "{text}");
    let extra = run(&["mcp", "x.docx"]);
    assert_eq!(extra.status.code(), Some(2));
    let text = String::from_utf8(extra.stderr).unwrap();
    assert!(text.starts_with("✗ "), "{text}");
}
