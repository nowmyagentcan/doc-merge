//! Writes merged fixtures to the folder in DOC_MERGE_OUT, for the Office, LibreOffice and
//! Open XML SDK checks that run outside `cargo test`:
//! `DOC_MERGE_OUT=dir cargo test --test office -- --ignored`.

use std::path::{Path, PathBuf};

use doc_merge::{Options, StyleMode, merge};

const MERGES: &[(&str, &[&str], StyleMode)] = &[
    ("abc.pptx", &["a.pptx", "b.pptx", "c.pptx"], StyleMode::Keep),
    ("aa.pptx", &["a.pptx", "a.pptx"], StyleMode::Keep),
    ("ca.pptx", &["c.pptx", "a.pptx"], StyleMode::Keep),
    ("ba.pptx", &["b.pptx", "a.pptx"], StyleMode::Keep),
    ("ab.xlsx", &["a.xlsx", "b.xlsx"], StyleMode::Keep),
    ("abb.xlsx", &["a.xlsx", "b.xlsx", "b.xlsx"], StyleMode::Keep),
    ("bb.xlsx", &["b.xlsx", "b.xlsx"], StyleMode::Keep),
    ("aa.xlsx", &["a.xlsx", "a.xlsx"], StyleMode::Keep),
    ("ba.xlsx", &["b.xlsx", "a.xlsx"], StyleMode::Keep),
    ("pa.xlsx", &["p.xlsx", "a.xlsx"], StyleMode::Keep),
    ("abc.docx", &["a.docx", "b.docx", "c.docx"], StyleMode::Keep),
    (
        "abc-first.docx",
        &["a.docx", "b.docx", "c.docx"],
        StyleMode::First,
    ),
    ("ca.docx", &["c.docx", "a.docx"], StyleMode::Keep),
    ("ba.docx", &["b.docx", "a.docx"], StyleMode::Keep),
    ("aa.docx", &["a.docx", "a.docx"], StyleMode::Keep),
    ("bb.docx", &["b.docx", "b.docx"], StyleMode::Keep),
    ("cab.docx", &["c.docx", "a.docx", "b.docx"], StyleMode::Keep),
];

#[test]
#[ignore = "writes files for checkers outside the suite"]
fn write_merged_fixtures() {
    let out = PathBuf::from(std::env::var("DOC_MERGE_OUT").expect("DOC_MERGE_OUT"));
    std::fs::create_dir_all(&out).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (name, inputs, styles) in MERGES {
        let target = out.join(name);
        let _ = std::fs::remove_file(&target);
        let paths: Vec<PathBuf> = inputs.iter().map(|i| fixtures.join(i)).collect();
        let options = Options { styles: *styles };
        merge(&paths, &target, &options, None).unwrap();
    }
}
