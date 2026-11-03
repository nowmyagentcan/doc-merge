//! Every error and warning a merge can end with: a stable code, one line saying what failed,
//! one line saying how to fix it, and the exit code it earns.

use serde::Serialize;

/// A failure, shown as two lines in text mode and as `{"error": …}` with `--json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<&'static str>,
    /// The 1-based input at fault, or none when no single input is.
    pub input: Option<usize>,
    #[serde(skip)]
    pub exit: u8,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

const RESAVE: &str = "Save it as .docx, .pptx or .xlsx.";
const SMALLER: &str = "Merge fewer or smaller files at a time.";
const TRY: &str = "Try: doc-merge a.pptx b.pptx -o all.pptx";

fn new(code: &'static str, exit: u8, message: String, hint: Option<&'static str>) -> Error {
    Error {
        code,
        message,
        hint,
        input: None,
        exit,
    }
}

/// What an `unsupported_format` file is instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreign {
    OpenDocument,
    Xlsb,
    OtherZip,
}

/// How many inputs of each family a run mixed: Word, PowerPoint, Excel.
pub type FamilyCounts = [usize; 3];

impl Error {
    /// The same error, blamed on the 1-based input `n`.
    pub fn at(mut self, n: usize) -> Self {
        self.input = Some(n);
        self
    }

    pub fn file_not_found(path: &str) -> Self {
        let m = format!("No file at '{path}'.");
        new("file_not_found", 1, m, Some("Check the path."))
    }

    pub fn no_match(pattern: &str) -> Self {
        let m = format!("No Office files match '{pattern}'.");
        new("no_match", 1, m, Some("Check the folder and the pattern."))
    }

    pub fn not_office_file(name: &str) -> Self {
        let m = format!("'{name}' isn't a Word, PowerPoint or Excel file.");
        let hint = "Use .docx, .pptx or .xlsx files (or .dotx, .potx, .ppsx, .xltx).";
        new("not_office_file", 1, m, Some(hint))
    }

    pub fn legacy_or_encrypted(name: &str) -> Self {
        let m = format!(
            "'{name}' is an old binary Office file (.doc, .xls, .ppt) or is password-protected."
        );
        let hint = "Open it in Office and save it as .docx, .pptx or .xlsx without a password.";
        new("legacy_or_encrypted", 1, m, Some(hint))
    }

    pub fn macro_enabled(name: &str) -> Self {
        let m = format!("'{name}' contains macros, which doc-merge doesn't merge.");
        let hint = "Save a copy without macros (.docx, .pptx or .xlsx) and merge that.";
        new("macro_enabled", 1, m, Some(hint))
    }

    pub fn strict_ooxml(name: &str) -> Self {
        let m = format!("'{name}' is saved as Strict Open XML.");
        let hint =
            "In Office, save it as a regular .docx, .pptx or .xlsx (not \"Strict Open XML\").";
        new("strict_ooxml", 1, m, Some(hint))
    }

    pub fn unsupported_format(name: &str, what: Foreign) -> Self {
        let what = match what {
            Foreign::OpenDocument => "an OpenDocument file",
            Foreign::Xlsb => "an Excel binary workbook (.xlsb)",
            Foreign::OtherZip => "a zip file but not an Office document",
        };
        let m = format!("'{name}' is {what}.");
        new("unsupported_format", 1, m, Some(RESAVE))
    }

    pub fn mixed_formats(counts: FamilyCounts) -> Self {
        let names = [
            ("Word document", "Word documents"),
            ("PowerPoint deck", "PowerPoint decks"),
            ("Excel workbook", "Excel workbooks"),
        ];
        let parts: Vec<String> = counts
            .iter()
            .zip(names)
            .filter(|(n, _)| **n > 0)
            .map(|(&n, (one, many))| format!("{n} {}", if n == 1 { one } else { many }))
            .collect();
        let m = format!(
            "The files mix {}; doc-merge merges one kind at a time.",
            and_list(&parts)
        );
        let hint =
            "Merge Word, PowerPoint and Excel files separately, e.g. doc-merge \"folder\\*.pptx\".";
        new("mixed_formats", 1, m, Some(hint))
    }

    /// `variant` is one of the names in the format contract, e.g. `pivot tables`.
    pub fn unsupported_variant(name: &str, variant: &str) -> Self {
        let hint = match variant {
            "pivot tables" => {
                "Remove the pivot tables (or paste their values) in Excel, save, and merge again."
            }
            "slicers" | "timelines" => "Remove the slicers or timelines in Excel and merge again.",
            "a data model" | "Power Query queries" | "data connections" | "query tables" => {
                "Remove the queries and data connections (Data › Queries & Connections), save, and merge again."
            }
            "images in cells" => {
                "Place the pictures over cells instead of in cells, or remove them, and merge again."
            }
            "slide or section zoom" => {
                "Replace the zoom with a normal link or remove it in PowerPoint, and merge again."
            }
            _ => "Remove it in Office, save, and merge again.",
        };
        let m = format!("'{name}' has {variant}, which doc-merge can't merge yet.");
        new("unsupported_variant", 1, m, Some(hint))
    }

    /// `name` uses the 1904 date system when `name_1904`, and `first` the other one.
    pub fn date_system_differs(name: &str, first: &str, name_1904: bool) -> Self {
        let (a, b) = if name_1904 {
            ("1904", "1900")
        } else {
            ("1900", "1904")
        };
        let m = format!(
            "'{name}' uses the {a} date system and '{first}' uses the {b} one; its dates would shift by four years."
        );
        let hint = "In Excel, set File › Options › Advanced › \"Use 1904 date system\" the same way in both workbooks, then merge.";
        new("date_system_differs", 1, m, Some(hint))
    }

    pub fn bad_package(name: &str, reason: &str) -> Self {
        let m = format!("'{name}' is damaged or unsafe: {reason}.");
        let hint = "Open and re-save it in Office, or use another file.";
        new("bad_package", 1, m, Some(hint))
    }

    /// e.g. `limit_exceeded("'big.pptx'", "600 MiB", "512 MiB")`.
    pub fn limit_exceeded(what: &str, value: &str, limit: &str) -> Self {
        let m = format!("{what} is over the limit: {value} (at most {limit}).");
        new("limit_exceeded", 1, m, Some(SMALLER))
    }

    pub fn output_exists(path: &str) -> Self {
        let m = format!("{path} already exists.");
        new(
            "output_exists",
            1,
            m,
            Some("Pass --overwrite, or choose another -o."),
        )
    }

    pub fn output_is_input(path: &str) -> Self {
        let m = format!("The output {path} is one of the files being merged.");
        new("output_is_input", 1, m, Some("Choose another -o."))
    }

    /// A usage error from the argument parser, with its own message.
    pub fn usage(message: String) -> Self {
        new("usage", 2, message, None)
    }

    pub fn need_two_files(found: usize) -> Self {
        let m = format!("Give at least two files to merge ({found} found).");
        new("need_two_files", 2, m, Some(TRY))
    }

    pub fn no_input() -> Self {
        new("no_input", 2, "No files given.".into(), Some(TRY))
    }

    pub fn bad_extension(ext: &str, stem: &str) -> Self {
        let m = format!("The output is a .{ext} file; name it {stem}.{ext}.");
        new("bad_extension", 2, m, None)
    }

    pub fn write_failed(path: &str, error: &std::io::Error) -> Self {
        new(
            "write_failed",
            3,
            format!("Couldn't write {path}: {error}."),
            None,
        )
    }

    pub fn timeout() -> Self {
        let hint = "Merge fewer files at once, or use the command line.";
        new("timeout", 3, "Stopped after 600 s.".into(), Some(hint))
    }

    pub fn port_in_use(port: u16) -> Self {
        let m = format!("Port {port} is in use.");
        new("port_in_use", 3, m, Some("Use --port 0 for a free port."))
    }

    pub fn internal(detail: &str) -> Self {
        let m = format!("Something went wrong inside doc-merge: {detail}.");
        let hint = "Run again with --debug and report it at https://github.com/nowmyagentcan/doc-merge/issues.";
        new("internal", 3, m, Some(hint))
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[String]) -> String {
    match items.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// A file size for messages, in whole MiB rounded up, so 512 MiB and one byte is `513 MiB`.
pub fn mib(bytes: u64) -> String {
    format!("{} MiB", thousands(bytes.div_ceil(1 << 20)))
}

/// `1234567` as `1,234,567`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Something the merge did that the user should know about; the exit code stays 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub code: &'static str,
    pub input: Option<usize>,
    pub message: String,
}

fn warn(code: &'static str, input: usize, message: String) -> Warning {
    Warning {
        code,
        input: Some(input),
        message,
    }
}

impl Warning {
    /// `from` and `to` are aspect ratios such as `4:3` and `16:9`.
    pub fn slides_scaled(input: usize, name: &str, from: &str, to: &str) -> Self {
        let m = format!(
            "Slides from {name} ({from}) were scaled to fit the {to} slide size and centred."
        );
        warn("slides_scaled", input, m)
    }

    pub fn theme_differs(input: usize, name: &str) -> Self {
        let m =
            format!("Shapes and charts from {name} use the first file's theme colours and fonts.");
        warn("theme_differs", input, m)
    }

    pub fn compat_mode_differs(input: usize, name: &str) -> Self {
        let m =
            format!("{name} was made for an older Word layout mode; its text may reflow slightly.");
        warn("compat_mode_differs", input, m)
    }

    pub fn toc_needs_update(input: usize) -> Self {
        let m = "The table of contents doesn't list the merged documents yet; in Word, right-click it and choose Update Field.";
        warn("toc_needs_update", input, m.into())
    }

    pub fn glossary_not_merged(input: usize, name: &str) -> Self {
        let m = format!(
            "The building blocks saved in {name} were left out (only the first file's are kept)."
        );
        warn("glossary_not_merged", input, m)
    }

    pub fn protection_not_kept(input: usize, name: &str) -> Self {
        let m = format!(
            "{name} was protected against editing; the merged file has the first file's protection settings only."
        );
        warn("protection_not_kept", input, m)
    }

    pub fn custom_views_dropped(input: usize, name: &str) -> Self {
        let m = format!("Custom views in {name} were left out.");
        warn("custom_views_dropped", input, m)
    }

    /// `parts` lists each dropped part type with its count, e.g. `[("task list", 2)]`.
    pub fn part_dropped(input: usize, name: &str, parts: &[(&str, usize)]) -> Self {
        let list: Vec<String> = parts
            .iter()
            .map(|(kind, n)| format!("{n} {kind} {}", if *n == 1 { "part" } else { "parts" }))
            .collect();
        let m = format!(
            "{name} has {} that the merged file doesn't carry (only the first file's are kept).",
            and_list(&list)
        );
        warn("part_dropped", input, m)
    }

    pub fn signature_removed(input: usize, name: &str) -> Self {
        let m = format!("The digital signature of {name} was removed; merging makes it invalid.");
        warn("signature_removed", input, m)
    }
}

/// The warnings of one run, in the order they were first met; a second warning with the same
/// code and input as an earlier one is left out.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Warnings(Vec<Warning>);

impl Warnings {
    pub fn push(&mut self, w: Warning) {
        if !self
            .0
            .iter()
            .any(|o| o.code == w.code && o.input == w.input)
        {
            self.0.push(w);
        }
    }

    pub fn list(&self) -> &[Warning] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_and_hints_follow_the_table() {
        let e = Error::unsupported_variant("q3.xlsx", "pivot tables").at(3);
        assert_eq!(
            e.message,
            "'q3.xlsx' has pivot tables, which doc-merge can't merge yet."
        );
        assert_eq!(
            e.hint,
            Some(
                "Remove the pivot tables (or paste their values) in Excel, save, and merge again."
            )
        );
        assert_eq!(
            (e.code, e.exit, e.input),
            ("unsupported_variant", 1, Some(3))
        );
        let e = Error::limit_exceeded("'huge.pptx'", &mib(572 << 20), "512 MiB");
        assert_eq!(
            e.message,
            "'huge.pptx' is over the limit: 572 MiB (at most 512 MiB)."
        );
        assert_eq!(mib((512 << 20) + 1), "513 MiB");
        assert_eq!(mib(3 << 30), "3,072 MiB");
        assert_eq!(thousands(999), "999");
        assert_eq!(
            Error::date_system_differs("b.xlsx", "a.xlsx", true).message,
            "'b.xlsx' uses the 1904 date system and 'a.xlsx' uses the 1900 one; its dates would shift by four years."
        );
        assert_eq!(
            Error::bad_extension("pptx", "all").message,
            "The output is a .pptx file; name it all.pptx."
        );
    }

    #[test]
    fn exit_codes_by_kind_of_failure() {
        assert_eq!(Error::output_exists("x").exit, 1);
        assert_eq!(Error::mixed_formats([1, 1, 0]).exit, 1);
        assert_eq!(Error::need_two_files(1).exit, 2);
        assert_eq!(Error::no_input().exit, 2);
        assert_eq!(Error::usage("bad flag".into()).exit, 2);
        let io = std::io::Error::other("denied");
        assert_eq!(Error::write_failed("x", &io).exit, 3);
        assert_eq!(Error::timeout().exit, 3);
        assert_eq!(Error::port_in_use(80).exit, 3);
        assert_eq!(Error::internal("x").exit, 3);
    }

    #[test]
    fn mixed_formats_counts_each_family() {
        let m = |c| Error::mixed_formats(c).message;
        assert_eq!(
            m([2, 3, 0]),
            "The files mix 2 Word documents and 3 PowerPoint decks; doc-merge merges one kind at a time."
        );
        assert_eq!(
            m([1, 1, 1]),
            "The files mix 1 Word document, 1 PowerPoint deck and 1 Excel workbook; doc-merge merges one kind at a time."
        );
    }

    #[test]
    fn equal_code_and_input_warnings_merge() {
        let mut w = Warnings::default();
        w.push(Warning::theme_differs(2, "b.docx"));
        w.push(Warning::theme_differs(2, "b.docx"));
        w.push(Warning::theme_differs(3, "c.docx"));
        w.push(Warning::toc_needs_update(2));
        assert_eq!(w.list().len(), 3);
        assert_eq!(
            Warning::part_dropped(2, "b.docx", &[("customizations", 1), ("task list", 2)]).message,
            "b.docx has 1 customizations part and 2 task list parts that the merged file doesn't carry (only the first file's are kept)."
        );
    }

    #[test]
    fn json_shapes() {
        let e = serde_json::to_string(&Error::output_is_input("C:\\a.pptx")).unwrap();
        assert_eq!(
            e,
            r#"{"code":"output_is_input","message":"The output C:\\a.pptx is one of the files being merged.","hint":"Choose another -o.","input":null}"#
        );
        let mut w = Warnings::default();
        w.push(Warning::signature_removed(1, "a.docx"));
        assert_eq!(
            serde_json::to_string(&w).unwrap(),
            r#"[{"code":"signature_removed","input":1,"message":"The digital signature of a.docx was removed; merging makes it invalid."}]"#
        );
    }
}
