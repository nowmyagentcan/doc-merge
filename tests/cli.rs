//! The binary: commands, paths, output files, text and JSON, exit codes and closed streams.

mod common;

use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{build, end_record, minimal, raw, raw_zip, set, temp_dir, write};
use serde_json::Value;

type Parts = Vec<(String, Vec<u8>)>;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_doc-merge"))
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    bin().current_dir(dir).args(args).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).unwrap()
}

fn json(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("{e}: {}", stdout(o)))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A fresh folder holding copies of `fixtures`.
fn folder(test: &str, fixtures: &[&str]) -> PathBuf {
    let dir = temp_dir(test);
    for f in fixtures {
        std::fs::copy(fixture(f), dir.join(f)).unwrap();
    }
    dir
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `args` fail with `code` and exit `exit`: two lines on stderr, nothing on stdout, and with
/// `--json` the same message on stdout as the error object and nothing on stderr.
fn refused(dir: &Path, args: &[&str], code: &str, exit: i32, message: &str, hint: Option<&str>) {
    let before = names_in(dir);
    let out = run_in(dir, args);
    assert_eq!(out.status.code(), Some(exit), "{code}: {}", stderr(&out));
    assert_eq!(stdout(&out), "", "{code}");
    let hint_line = hint.map_or(String::new(), |h| format!("  {h}\n"));
    assert_eq!(stderr(&out), format!("✗ {message}\n{hint_line}"), "{code}");

    let out = run_in(dir, &[args, &["--json"]].concat());
    assert_eq!(out.status.code(), Some(exit), "{code}");
    assert_eq!(stderr(&out), "", "{code}");
    let e = json(&out);
    assert_eq!(e.as_object().unwrap().len(), 1, "{e}");
    assert_eq!(e["error"]["code"], code);
    assert_eq!(e["error"]["message"], message);
    assert_eq!(e["error"]["hint"].as_str(), hint);
    assert!(e["error"].as_object().unwrap().contains_key("input"));
    assert_eq!(names_in(dir), before, "{code} wrote nothing");
}

#[test]
fn version_prints_name_and_version() {
    for flag in ["--version", "-V"] {
        let out = bin().arg(flag).output().unwrap();
        assert!(out.status.success());
        assert_eq!(stdout(&out), "doc-merge 0.1.0\n");
    }
}

#[test]
fn help_shows_the_commands_what_is_kept_and_the_examples() {
    let out = bin().arg("--help").output().unwrap();
    assert!(out.status.success());
    let help = stdout(&out);
    assert!(help.starts_with(
        "Merge PowerPoint, Excel or Word files.\n\nUsage: doc-merge [merge] <PATH>... [OPTIONS]\n"
    ));
    for line in [
        "  ui     Open the local page in your browser\n",
        "  -o, --output PATH   Output file or folder (default: merged.<ext> beside the first file)\n",
        "What each kind keeps:\n",
        "Refused: macros, password-protected and old binary files, Strict Open XML, ODF, .xlsb;\n",
        "  doc-merge deck-a.pptx deck-b.pptx -o all.pptx\n",
        "  doc-merge cover.docx chapters\\ appendix.docx -o book.docx\n",
        "  doc-merge \"reports\\*.docx\" --styles first\n",
    ] {
        assert!(help.contains(line), "{line}");
    }
    assert_eq!(stdout(&bin().arg("-h").output().unwrap()), help);
    assert_eq!(
        stdout(&bin().args(["merge", "--help"]).output().unwrap()),
        help
    );
    let out = bin().args(["ui", "--help"]).output().unwrap();
    assert!(out.status.success());
    assert!(stdout(&out).contains("--no-open"), "{}", stdout(&out));
}

#[test]
fn usage_errors_exit_2() {
    let dir = temp_dir("usage");
    let hint = Some("See doc-merge --help.");
    refused(
        &dir,
        &["a.pptx", "--x"],
        "usage",
        2,
        "Unknown option --x.",
        hint,
    );
    refused(
        &dir,
        &["a.pptx", "--port", "1"],
        "usage",
        2,
        "Unknown option --port.",
        hint,
    );
    let out = run_in(&dir, &["a.pptx", "b.pptx", "--styles", "odd", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json(&out)["error"]["code"], "usage");
    let message = json(&out)["error"]["message"].as_str().unwrap().to_string();
    assert!(
        message.starts_with("Invalid value 'odd' for '--styles"),
        "{message}"
    );
    assert!(message.ends_with('.'), "{message}");
    let out = run_in(&dir, &["-o", "a", "-o", "b", "x", "y"]);
    assert_eq!(out.status.code(), Some(2));
    let out = run_in(&dir, &["ui", "--port", "70000"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).starts_with("✗ "), "{}", stderr(&out));
    assert!(
        stderr(&out).ends_with("  See doc-merge --help.\n"),
        "{}",
        stderr(&out)
    );

    let try_it = Some("Try: doc-merge a.pptx b.pptx -o all.pptx");
    refused(&dir, &[], "no_input", 2, "No files given.", try_it);
    refused(&dir, &["merge"], "no_input", 2, "No files given.", try_it);
    let one = "Give at least two files to merge (1 found).";
    refused(&dir, &["a.pptx"], "need_two_files", 2, one, try_it);
}

#[test]
fn the_first_argument_is_a_command_only_when_exact() {
    let dir = folder("commands", &["a.pptx", "b.pptx"]);
    let sep = MAIN_SEPARATOR;
    std::fs::rename(dir.join("a.pptx"), dir.join("merge")).unwrap();
    std::fs::rename(dir.join("b.pptx"), dir.join("ui")).unwrap();
    assert!(
        run_in(&dir, &[&format!(".{sep}merge"), "ui", "-o", "1.pptx"])
            .status
            .success()
    );
    assert!(
        run_in(&dir, &["merge", "merge", "ui", "-o", "2.pptx"])
            .status
            .success()
    );
    assert!(
        run_in(&dir, &["-o", "3.pptx", "--", "merge", "ui"])
            .status
            .success()
    );
    assert_eq!(
        run_in(&dir, &["--json", "merge", "ui"]).status.code(),
        Some(0)
    );
    assert!(dir.join("merged.pptx").is_file());
}

#[test]
fn options_go_anywhere_and_after_two_dashes_all_is_a_path() {
    let dir = folder("options", &["a.pptx", "b.pptx"]);
    let out = run_in(&dir, &["--json", "a.pptx", "-o", "1.pptx", "b.pptx"]);
    assert_eq!(json(&out)["inputs"].as_array().unwrap().len(), 2);
    std::fs::rename(dir.join("b.pptx"), dir.join("-b.pptx")).unwrap();
    let out = run_in(&dir, &["--json", "-o", "2.pptx", "--", "a.pptx", "-b.pptx"]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(dir.join("2.pptx").is_file());
    let e = run_in(&dir, &["--json", "--", "a.pptx", "--overwrite"]);
    assert_eq!(json(&e)["error"]["code"], "file_not_found");
    assert_eq!(json(&e)["error"]["input"], 2);
}

#[test]
fn folders_and_patterns_keep_name_order() {
    let dir = temp_dir("paths");
    let decks = dir.join("decks");
    std::fs::create_dir(&decks).unwrap();
    for (from, to) in [
        ("a.pptx", "deck2.pptx"),
        ("b.pptx", "deck10.pptx"),
        ("a.pptx", "~$deck2.pptx"),
        ("a.pptx", "merged.pptx"),
    ] {
        std::fs::copy(fixture(from), decks.join(to)).unwrap();
    }
    let inputs = |o: &Output| -> Vec<String> {
        let list = json(o)["inputs"].as_array().unwrap().clone();
        let name = |v: &Value| {
            let p = PathBuf::from(v["path"].as_str().unwrap());
            p.file_name().unwrap().to_string_lossy().into_owned()
        };
        list.iter().map(name).collect()
    };
    let out = run_in(&dir, &["decks", "--json", "-o", "1.pptx"]);
    assert_eq!(inputs(&out), ["deck2.pptx", "deck10.pptx"]);
    let pattern = format!("decks{MAIN_SEPARATOR}*.pptx");
    let out = run_in(&dir, &[&pattern, "--json", "-o", "2.pptx"]);
    assert_eq!(inputs(&out), ["deck2.pptx", "deck10.pptx"]);
    let out = run_in(&dir, &[&pattern, "decks", "--json", "-o", "3.pptx"]);
    assert_eq!(inputs(&out).len(), 4);

    let none = format!("decks{MAIN_SEPARATOR}*.docx");
    refused(
        &dir,
        &[&none],
        "no_match",
        1,
        &format!("No Office files match '{none}'."),
        Some("Check the folder and the pattern."),
    );
}

#[test]
fn outputs_get_a_free_name_and_never_overwrite_without_asking() {
    let dir = folder("outputs", &["a.pptx", "b.pptx"]);
    let path = |o: &Output| PathBuf::from(json(o)["output"]["path"].as_str().unwrap());
    let args = ["a.pptx", "b.pptx", "--json"];
    let first = run_in(&dir, &args);
    let at = std::path::absolute(dir.join("merged.pptx")).unwrap();
    assert_eq!(path(&first), at);
    assert_eq!(
        path(&run_in(&dir, &args)).file_name().unwrap(),
        "merged-2.pptx"
    );

    let out = run_in(&dir, &[&args[..], &["-o", "merged.pptx"]].concat());
    assert_eq!(json(&out)["error"]["code"], "output_exists");
    let replaced = run_in(
        &dir,
        &[&args[..], &["-o", "merged.pptx", "--overwrite"]].concat(),
    );
    assert_eq!(path(&replaced), at);

    std::fs::create_dir(dir.join("sub")).unwrap();
    let out = run_in(&dir, &[&args[..], &["-o", "sub"]].concat());
    assert_eq!(path(&out).parent().unwrap().file_name().unwrap(), "sub");
    let new = format!("new{MAIN_SEPARATOR}");
    run_in(&dir, &[&args[..], &["-o", &new]].concat());
    assert!(dir.join("new/merged.pptx").is_file());

    let beside = temp_dir("beside");
    let a = dir.join("a.pptx");
    let out = run_in(
        &beside,
        &[a.to_str().unwrap(), a.to_str().unwrap(), "--json"],
    );
    assert_eq!(
        path(&out).parent().unwrap(),
        std::path::absolute(&dir).unwrap()
    );
    assert!(names_in(&beside).is_empty());
}

#[test]
fn the_output_kind_decides_the_extension_and_an_input_is_never_replaced() {
    let dir = folder("kinds", &["a.pptx", "b.pptx", "a.docx"]);
    refused(
        &dir,
        &["a.pptx", "b.pptx", "-o", "all.docx"],
        "bad_extension",
        2,
        "The output is a .pptx file; name it all.pptx.",
        None,
    );
    // The highest exit code wins over a refusal of a later file.
    refused(
        &dir,
        &["a.pptx", "gone.pptx", "-o", "all.txt"],
        "bad_extension",
        2,
        "The output is a .pptx file; name it all.pptx.",
        None,
    );
    for args in [
        vec!["a.pptx", "b.pptx", "-o", "b.pptx"],
        vec!["a.pptx", "b.pptx", "-o", "b.pptx", "--overwrite"],
    ] {
        refused(
            &dir,
            &args,
            "output_is_input",
            1,
            "The output b.pptx is one of the files being merged.",
            Some("Choose another -o."),
        );
    }
    let bytes = std::fs::read(dir.join("a.docx")).unwrap();
    write(&dir, "taken.pptx", &bytes);
    refused(
        &dir,
        &["a.pptx", "b.pptx", "-o", "taken.pptx"],
        "output_exists",
        1,
        "taken.pptx already exists.",
        Some("Pass --overwrite, or choose another -o."),
    );
    let gone = format!("gone{MAIN_SEPARATOR}x.pptx");
    let out = run_in(&dir, &["a.pptx", "b.pptx", "-o", &gone, "--json"]);
    assert_eq!(out.status.code(), Some(3));
    let e = json(&out)["error"].clone();
    assert_eq!(e["code"], "write_failed");
    assert_eq!(e["hint"], Value::Null);
    let message = e["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!("Couldn't write {gone}: ")),
        "{message}"
    );
}

#[test]
fn every_refusal_of_an_input_has_its_lines_and_code() {
    let dir = folder("codes", &["a.docx", "a.pptx", "a.xlsx", "c.xlsx", "p.xlsx"]);
    let file = |name: &str, bytes: &[u8]| {
        write(&dir, name, bytes);
    };
    let package = |kind: &str, patch: &dyn Fn(&mut Parts)| {
        let mut parts = minimal(kind, &[]);
        patch(&mut parts);
        build(&parts)
    };
    file("note.txt", b"plain text");
    file(
        "old.doc",
        &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0],
    );
    let macros = package("docx", &|p| {
        let types = &p.iter().find(|x| x.0 == "[Content_Types].xml").unwrap().1;
        let types = String::from_utf8(types.clone()).unwrap().replace(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            "application/vnd.ms-word.document.macroEnabled.main+xml",
        );
        set(p, "[Content_Types].xml", types.as_bytes());
    });
    file("macro.docm", &macros);
    let strict = package("docx", &|p| {
        let rels = String::from_utf8(p.iter().find(|x| x.0 == "_rels/.rels").unwrap().1.clone())
            .unwrap()
            .replace(
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument",
                "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument",
            );
        set(p, "_rels/.rels", rels.as_bytes());
    });
    file("strict.docx", &strict);
    file(
        "o.odt",
        &common::zip(&[("mimetype", b"application/vnd.oasis.opendocument.text")]),
    );
    let bad = raw_zip(&[raw("../x.xml", b"x")]);
    file("bad.docx", &bad);
    let mut hostile = raw_zip(&[]);
    hostile.truncate(hostile.len() - 22);
    hostile.extend(end_record(10_001, 0, 0));
    file("many.docx", &hostile);

    let rows: [(&[&str], &str, String, &str); 11] = [
        (
            &["a.pptx", "gone.pptx"],
            "file_not_found",
            "No file at 'gone.pptx'.".into(),
            "Check the path.",
        ),
        (
            &["note.txt", "a.docx"],
            "not_office_file",
            "'note.txt' isn't a Word, PowerPoint or Excel file.".into(),
            "Use .docx, .pptx or .xlsx files (or .dotx, .potx, .ppsx, .xltx).",
        ),
        (
            &["a.docx", "old.doc"],
            "legacy_or_encrypted",
            "'old.doc' is an old binary Office file (.doc, .xls, .ppt) or is password-protected."
                .into(),
            "Open it in Office and save it as .docx, .pptx or .xlsx without a password.",
        ),
        (
            &["a.docx", "macro.docm"],
            "macro_enabled",
            "'macro.docm' contains macros, which doc-merge doesn't merge.".into(),
            "Save a copy without macros (.docx, .pptx or .xlsx) and merge that.",
        ),
        (
            &["strict.docx", "a.docx"],
            "strict_ooxml",
            "'strict.docx' is saved as Strict Open XML.".into(),
            "In Office, save it as a regular .docx, .pptx or .xlsx (not \"Strict Open XML\").",
        ),
        (
            &["a.docx", "o.odt"],
            "unsupported_format",
            "'o.odt' is an OpenDocument file.".into(),
            "Save it as .docx, .pptx or .xlsx.",
        ),
        (
            &["a.docx", "a.pptx", "a.xlsx"],
            "mixed_formats",
            "The files mix 1 Word document, 1 PowerPoint deck and 1 Excel workbook; doc-merge merges one kind at a time.".into(),
            "Merge Word, PowerPoint and Excel files separately, e.g. doc-merge \"folder\\*.pptx\".",
        ),
        (
            &["a.xlsx", "p.xlsx"],
            "unsupported_variant",
            "'p.xlsx' has pivot tables, which doc-merge can't merge yet.".into(),
            "Remove the pivot tables (or paste their values) in Excel, save, and merge again.",
        ),
        (
            &["a.xlsx", "c.xlsx"],
            "date_system_differs",
            "'c.xlsx' uses the 1904 date system and 'a.xlsx' uses the 1900 one; its dates would shift by four years.".into(),
            "In Excel, set File › Options › Advanced › \"Use 1904 date system\" the same way in both workbooks, then merge.",
        ),
        (
            &["a.docx", "bad.docx"],
            "bad_package",
            "'bad.docx' is damaged or unsafe: part name ../x.xml is not allowed.".into(),
            "Open and re-save it in Office, or use another file.",
        ),
        (
            &["a.docx", "many.docx"],
            "limit_exceeded",
            "The entry count of 'many.docx' is over the limit: 10,001 (at most 10,000).".into(),
            "Merge fewer or smaller files at a time.",
        ),
    ];
    for (args, code, message, hint) in rows {
        refused(&dir, args, code, 1, &message, Some(hint));
    }
    let out = run_in(&dir, &["a.docx", "macro.docm", "--json"]);
    assert_eq!(json(&out)["error"]["input"], 2);
    let out = run_in(&dir, &["a.docx", "a.pptx", "--json"]);
    assert_eq!(json(&out)["error"]["input"], Value::Null);

    let many = temp_dir("too-many");
    let name = |i: usize| format!("{i}.docx");
    for i in 0..501 {
        write(&many, &name(i), b"");
    }
    refused(
        &many,
        &["."],
        "limit_exceeded",
        1,
        "The number of files is over the limit: 501 (at most 500).",
        Some("Merge fewer or smaller files at a time."),
    );
}

/// The keys of an object in the order they were printed.
fn key_order(text: &str, keys: &[&str]) {
    let at: Vec<usize> = keys
        .iter()
        .map(|k| {
            text.find(&format!("\"{k}\""))
                .unwrap_or_else(|| panic!("{k} in {text}"))
        })
        .collect();
    assert!(at.is_sorted(), "{keys:?} in {text}");
}

#[test]
fn json_has_the_documented_shape_for_each_family() {
    let dir = folder(
        "json",
        &["a.pptx", "c.pptx", "a.xlsx", "b.xlsx", "a.docx", "b.docx"],
    );
    let path = |s: &Value| PathBuf::from(s.as_str().unwrap());

    let out = run_in(&dir, &["a.pptx", "c.pptx", "--json"]);
    assert!(out.status.success());
    let text = stdout(&out);
    key_order(
        &text,
        &[
            "output",
            "path",
            "kind",
            "bytes",
            "inputs",
            "slides",
            "output_slides",
            "renames",
        ],
    );
    key_order(
        &text,
        &["renames", "renames_omitted", "warnings", "elapsed_ms"],
    );
    let v = json(&out);
    assert_eq!(v.as_object().unwrap().len(), 6);
    assert!(path(&v["output"]["path"]).is_absolute());
    assert_eq!(v["output"]["kind"], "pptx");
    let size = std::fs::metadata(dir.join("merged.pptx")).unwrap().len();
    assert_eq!(v["output"]["bytes"], size);
    let inputs = v["inputs"].as_array().unwrap();
    assert!(path(&inputs[1]["path"]).ends_with("c.pptx"));
    assert!(path(&inputs[1]["path"]).is_absolute());
    assert_eq!(inputs[0]["kind"], "pptx");
    let slides = inputs[0]["slides"].as_u64().unwrap();
    assert_eq!(inputs[0]["output_slides"], serde_json::json!([1, slides]));
    assert_eq!(inputs[1]["output_slides"][0], slides + 1);
    let rename = &v["renames"][0];
    assert_eq!(rename["input"], 2);
    assert_eq!(rename["kind"], "custom_show");
    assert_eq!(rename["from"], "Short");
    assert_eq!(rename["to"], "Short (2)");
    let list = &text[text.find("\"renames\"").unwrap()..];
    key_order(list, &["input", "kind", "from", "to", "code", "message"]);
    assert_eq!(v["renames_omitted"], 0);
    let w = &v["warnings"][0];
    assert_eq!(w["code"], "slides_scaled");
    assert_eq!(w["input"], 2);
    assert!(
        w["message"]
            .as_str()
            .unwrap()
            .starts_with("Slides from c.pptx (4:3)")
    );
    assert!(v["elapsed_ms"].is_u64());

    let v = json(&run_in(&dir, &["a.xlsx", "b.xlsx", "--json"]));
    assert_eq!(v["output"]["kind"], "xlsx");
    let sheets = &v["inputs"][1]["sheets"];
    assert_eq!(sheets[0]["name"], "Summary");
    assert_eq!(sheets[0]["output_name"], "Summary (2)");
    let names: Vec<&str> = v["renames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"sheet"), "{names:?}");

    let v = json(&run_in(&dir, &["a.docx", "b.docx", "--json"]));
    assert_eq!(v["output"]["kind"], "docx");
    let sections = v["inputs"][0]["sections"].as_u64().unwrap();
    assert_eq!(
        v["inputs"][0]["output_sections"],
        serde_json::json!([1, sections])
    );
    assert_eq!(v["inputs"][1]["output_sections"][0], sections + 1);
    let codes: Vec<&str> = v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"toc_needs_update"), "{codes:?}");
    assert_eq!(v["warnings"][0]["input"], 2);
}

#[test]
fn styles_first_reaches_the_merge() {
    let dir = folder("styles", &["a.docx", "b.docx"]);
    let keep = json(&run_in(
        &dir,
        &["a.docx", "b.docx", "--json", "--styles", "keep"],
    ));
    let first = json(&run_in(
        &dir,
        &["a.docx", "b.docx", "--json", "--styles", "first"],
    ));
    let styles = |v: &Value| {
        let list = v["renames"].as_array().unwrap();
        list.iter().filter(|r| r["kind"] == "style").count()
    };
    assert!(styles(&keep) > 0);
    assert_eq!(styles(&first), 0);
}

#[test]
fn a_thousand_renames_are_listed_and_the_rest_counted() {
    let dir = temp_dir("renames");
    let marks: String = (0..1_100)
        .map(|i| {
            format!(r#"<w:bookmarkStart w:id="{i}" w:name="m{i}"/><w:bookmarkEnd w:id="{i}"/>"#)
        })
        .collect();
    let doc = format!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p>{marks}</w:p></w:body></w:document>"#
    );
    let mut parts = minimal("docx", &[]);
    set(&mut parts, "word/document.xml", doc.as_bytes());
    write(&dir, "m.docx", &build(&parts));
    let out = run_in(&dir, &["m.docx", "m.docx", "--json"]);
    let v = json(&out);
    assert_eq!(v["renames"].as_array().unwrap().len(), 1_000);
    assert_eq!(v["renames_omitted"], 100);
    assert_eq!(v["renames"][999]["kind"], "bookmark");
}

#[test]
fn debug_adds_timings_and_causes_on_stderr_only() {
    let dir = folder("debug", &["a.pptx", "b.pptx"]);
    let out = run_in(&dir, &["a.pptx", "b.pptx", "--debug", "--json"]);
    assert!(out.status.success());
    assert!(json(&out)["output"].is_object());
    let lines: Vec<String> = stderr(&out).lines().map(String::from).collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    for (line, step) in lines.iter().zip(["open", "plan", "write"]) {
        assert!(line.starts_with(&format!("debug: {step} ")), "{line}");
        assert!(line.ends_with(" ms"), "{line}");
    }

    let cut = &std::fs::read(dir.join("a.pptx")).unwrap()[..2000];
    let mut damaged = cut.to_vec();
    damaged.extend(end_record(1, 46, 0));
    write(&dir, "cut.pptx", &damaged);
    let out = run_in(&dir, &["cut.pptx", "a.pptx", "--debug"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.starts_with("debug: bad_package: "), "{err}");
    assert!(
        err.contains("\n✗ 'cut.pptx' is damaged or unsafe: "),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn ctrl_c_stops_a_merge_and_leaves_no_temp_file() {
    let dir = folder("ctrl-c", &["a.xlsx"]);
    let mut parts = common::entries(&dir.join("a.xlsx"));
    let sheet = parts
        .iter()
        .position(|p| p.0.ends_with("sheet1.xml"))
        .unwrap();
    let rows: String = (1_000..400_000)
        .map(|i| format!(r#"<row r="{i}"><c r="A{i}"><v>1</v></c></row>"#))
        .collect();
    let text = String::from_utf8(parts[sheet].1.clone()).unwrap();
    parts[sheet].1 = text
        .replace("</sheetData>", &(rows + "</sheetData>"))
        .into_bytes();
    write(&dir, "big.xlsx", &build(&parts));

    let mut merge = bin()
        .current_dir(&dir)
        .args(["big.xlsx", "big.xlsx"])
        .spawn()
        .unwrap();
    let temp = || names_in(&dir).iter().any(|n| n.ends_with(".tmp"));
    while !temp() {
        assert_eq!(merge.try_wait().unwrap(), None, "ended before writing");
    }
    let id = merge.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &id])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(merge.wait().unwrap().code(), Some(130));
    assert_eq!(names_in(&dir), ["a.xlsx", "big.xlsx"]);
}

#[test]
fn text_output_is_plain_lines_without_a_terminal() {
    let dir = folder("text", &["a.pptx", "c.pptx", "a.xlsx", "b.xlsx"]);
    let out = bin()
        .current_dir(&dir)
        .args(["a.pptx", "c.pptx", "-o", "all.pptx"])
        .env_remove("NO_COLOR")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(stderr(&out), "");
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(
        lines[0].starts_with("✓ all.pptx  7 slides from 2 files  "),
        "{text}"
    );
    assert_eq!(
        lines[1],
        "  renamed  custom show Short → Short (2)  (c.pptx)"
    );
    assert_eq!(
        lines[2],
        "! Slides from c.pptx (4:3) were scaled to fit the 16:9 slide size and centred."
    );

    // A path outside the current folder stays whole.
    let elsewhere = temp_dir("elsewhere");
    let target = std::path::absolute(elsewhere.join("x.xlsx")).unwrap();
    let out = run_in(&dir, &["a.xlsx", "b.xlsx", "-o", target.to_str().unwrap()]);
    let first = format!("✓ {}  4 sheets from 2 files  ", target.display());
    assert!(stdout(&out).starts_with(&first), "{}", stdout(&out));
}

#[test]
fn names_print_safely_and_json_keeps_them() {
    let dir = folder("names", &["a.pptx", "b.pptx"]);
    let name = "a\u{202E}x.pptx";
    std::fs::rename(dir.join("a.pptx"), dir.join(name)).unwrap();
    let out = run_in(&dir, &[name, "b.pptx", "-o", "t\u{2066}.pptx"]);
    let text = stdout(&out);
    assert!(text.starts_with("✓ t<U+2066>.pptx  "), "{text}");
    let out = run_in(&dir, &[name, "gone.pptx", "--json"]);
    assert_eq!(json(&out)["error"]["input"], 2);
    let out = run_in(&dir, &[name, name, "--json"]);
    let input = json(&out)["inputs"][0]["path"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(input.ends_with(name), "{input}");

    write(&dir, "note\u{202E}.txt", b"text");
    let out = run_in(&dir, &["note\u{202E}.txt", name]);
    assert!(
        stderr(&out).starts_with("✗ 'note<U+202E>.txt' isn't"),
        "{}",
        stderr(&out)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_name_that_isnt_unicode_merges_and_prints() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = folder("bytes", &["a.pptx"]);
    let bytes = OsStr::from_bytes(b"caf\xE9\x1b.pptx");
    std::fs::copy(dir.join("a.pptx"), dir.join(bytes)).unwrap();
    let args = [OsStr::new("a.pptx"), bytes, OsStr::new("--json")];
    let out = bin().current_dir(&dir).args(args).output().unwrap();
    let path = json(&out)["inputs"][1]["path"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(path.ends_with("caf\u{FFFD}\u{1b}.pptx"), "{path}");
    let out = bin()
        .current_dir(&dir)
        .args([bytes, OsStr::new("gone")])
        .output()
        .unwrap();
    assert!(
        stderr(&out).contains("No file at 'gone'"),
        "{}",
        stderr(&out)
    );
}

/// Runs with stdout or stderr a pipe whose reading end is already closed.
fn closed(dir: &Path, args: &[&str], stdout: bool) -> Output {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut c = bin();
    c.current_dir(dir).args(args);
    if stdout {
        c.stdout(writer).stderr(Stdio::piped());
    } else {
        c.stderr(writer).stdout(Stdio::piped());
    }
    c.output().unwrap()
}

#[test]
fn a_closed_stream_ends_quietly_with_the_code_earned() {
    let dir = folder("closed", &["a.pptx", "b.pptx", "c.xlsx", "a.xlsx"]);
    let cases: [(&[&str], i32); 6] = [
        (&["a.pptx", "b.pptx", "-o", "1.pptx", "--overwrite"], 0),
        (
            &["a.pptx", "b.pptx", "-o", "1.pptx", "--overwrite", "--json"],
            0,
        ),
        (&["a.pptx", "gone.pptx"], 1),
        (&["a.xlsx", "c.xlsx", "--json"], 1),
        (&["a.pptx", "--x"], 2),
        (&["--help"], 0),
    ];
    for (args, code) in cases {
        for extra in [&[][..], &["--debug"][..]] {
            let args = [args, extra].concat();
            for on_stdout in [true, false] {
                let out = closed(&dir, &args, on_stdout);
                assert_eq!(out.status.code(), Some(code), "{args:?} {on_stdout}");
                let left = [stderr(&out), stdout(&out)].concat();
                assert!(!left.contains("panicked"), "{left}");
            }
        }
    }
    let out = closed(&dir, &["a.pptx", "b.pptx", "-o", "2.pptx"], true);
    assert!(out.status.success());
    assert!(dir.join("2.pptx").is_file(), "the work still gets done");
}

#[cfg(target_os = "linux")]
fn in_a_terminal(dir: &Path, line: &str, no_color: bool) -> String {
    let mut c = Command::new("script");
    c.args(["-qec", line, "/dev/null"]).current_dir(dir);
    if no_color {
        c.env("NO_COLOR", "1");
    } else {
        c.env_remove("NO_COLOR");
    }
    let out = c.stdin(Stdio::null()).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(target_os = "linux")]
#[test]
fn colour_needs_a_terminal_and_no_no_color() {
    let dir = folder("colour", &["a.pptx", "b.pptx"]);
    let exe = env!("CARGO_BIN_EXE_doc-merge");
    let screen = in_a_terminal(&dir, &format!("{exe} a.pptx b.pptx -o 1.pptx"), false);
    assert!(
        screen.contains("\x1b[32m✓\x1b[0m \x1b[1m1.pptx\x1b[0m"),
        "{screen:?}"
    );
    let screen = in_a_terminal(&dir, &format!("{exe} a.pptx gone.pptx"), false);
    assert!(
        screen.contains("\x1b[31m✗\x1b[0m No file at 'gone.pptx'."),
        "{screen:?}"
    );
    let screen = in_a_terminal(&dir, &format!("{exe} a.pptx b.pptx -o 2.pptx"), true);
    assert!(
        screen.contains("✓ 2.pptx  ") && !screen.contains('\x1b'),
        "{screen:?}"
    );
    let screen = in_a_terminal(
        &dir,
        &format!("{exe} a.pptx b.pptx -o 3.pptx --json"),
        false,
    );
    assert!(!screen.contains('\x1b'), "{screen:?}");
    let empty = in_a_terminal(&dir, &format!("NO_COLOR= {exe} a.pptx gone.pptx"), false);
    assert!(empty.contains("\x1b[31m✗"), "{empty:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_terminal_with_no_arguments_gets_the_prompt() {
    use std::io::Write;
    let dir = folder("prompt", &["a.pptx", "b.pptx"]);
    let mut session = Command::new("script")
        .args(["-qec", env!("CARGO_BIN_EXE_doc-merge"), "/dev/null"])
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut typed = session.stdin.take().unwrap();
    typed
        .write_all(b"/help\n\"a.pptx\" b.pptx\na.pptx\n/nope\n/quit\n")
        .unwrap();
    let out = session.wait_with_output().unwrap();
    let screen = stdout(&out).replace("\r\n", "\n");
    for line in [
        "│ ✻ doc-merge 0.1.0                        │\n",
        "│   Merge PowerPoint, Excel or Word files. │\n",
        "  Drop files here or type paths · /help · /quit\n",
        "  /help    show this\n",
        "  ✓ merged.pptx  ",
        "  ✗ Give at least two files to merge (1 found).\n    Type two or more paths on one line.\n",
        "  ✗ Unknown command '/nope'.\n    Try /help.\n",
    ] {
        assert!(screen.contains(line), "{line} in {screen}");
    }
    assert!(dir.join("merged.pptx").is_file());
}

#[test]
fn no_arguments_without_a_terminal_are_a_usage_error() {
    let out = bin().stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr(&out),
        "✗ No files given.\n  Try: doc-merge a.pptx b.pptx -o all.pptx\n"
    );
}
