mod common;

use std::path::{Path, PathBuf};

use common::*;
use doc_merge::{Report, merge};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Merges fixtures into a fresh folder; returns the report and the output's parts.
fn merged(test: &str, names: &[&str]) -> (Report, Vec<(String, Vec<u8>)>) {
    let out = temp_dir(test).join("out.docx");
    let paths: Vec<PathBuf> = names.iter().map(|n| fixture(n)).collect();
    let report = merge(&paths, &out, None).unwrap();
    let parts = entries(&out);
    (report, parts)
}

/// The text of every `w:t` in a part, joined.
fn words(xml: &str) -> String {
    xml.split("<w:t")
        .skip(1)
        .filter_map(|s| Some(&s[s.find('>')? + 1..s.find("</w:t>")?]))
        .collect()
}

fn count(s: &str, what: &str) -> usize {
    s.matches(what).count()
}

#[test]
fn bodies_join_in_order_with_their_sections() {
    let (report, parts) = merged("join", &["a.docx", "b.docx", "c.docx"]);
    let doc = text(&parts, "word/document.xml");
    let said = words(&doc);
    let at = |s: &str| said.find(s).unwrap_or_else(|| panic!("{s} missing"));
    assert!(at("Document A") < at("Title of B"));
    assert!(at("Clause B one.") < at("Document C"));
    let at = |s: &str| doc.find(s).unwrap_or_else(|| panic!("{s} missing"));
    assert!(at("The end of A.") < at("orient=\"landscape\""));
    assert!(at("orient=\"landscape\"") < at("Title of B"));
    // a: 2 sections, b: 1, c: 1; the body's own break is c's, the others sit in paragraphs.
    let sections: Vec<_> = report.inputs.iter().map(|i| i.output_sections).collect();
    assert_eq!(sections, [Some([1, 2]), Some([3, 3]), Some([4, 4])]);
    assert_eq!(count(&doc, "<w:sectPr"), 4);
    let body_end = &doc[doc.rfind("</w:tbl>").unwrap()..];
    assert!(
        body_end.starts_with("</w:tbl><w:sectPr"),
        "c ends in its table"
    );
    assert!(!doc.contains("w:val=\"continuous\""));
    // Every relationship id in the body resolves (the writer checks the rest).
    let rels = text(&parts, "word/_rels/document.xml.rels");
    for id in doc
        .split("r:id=\"")
        .skip(1)
        .map(|s| &s[..s.find('"').unwrap()])
    {
        assert!(rels.contains(&format!("Id=\"{id}\"")), "{id}");
    }
}

#[test]
fn a_body_ending_in_a_table_gets_a_paragraph_for_its_break() {
    let (_, parts) = merged("table-end", &["c.docx", "a.docx"]);
    let doc = text(&parts, "word/document.xml");
    let after = &doc[doc.find("</w:tbl>").unwrap()..];
    assert!(
        after.starts_with("</w:tbl><w:p><w:pPr><w:sectPr"),
        "{}",
        &after[..80]
    );
}

#[test]
fn the_same_picture_is_stored_once() {
    let (_, parts) = merged("media", &["a.docx", "b.docx"]);
    let picture = &entries(&fixture("b.docx"))
        .into_iter()
        .find(|e| e.0 == "word/media/image1.png")
        .unwrap()
        .1;
    let copies = parts.iter().filter(|e| &e.1 == picture).count();
    assert_eq!(copies, 1);
}

/// Every output part is either one the merge rewrites or a byte-identical copy of an input's.
#[test]
fn untouched_parts_are_copied_byte_for_byte_and_runs_repeat() {
    let names = ["a.docx", "b.docx", "c.docx"];
    let (_, parts) = merged("fidelity", &names);
    let sources: Vec<Vec<u8>> = names
        .iter()
        .flat_map(|n| entries(&fixture(n)))
        .map(|e| e.1)
        .collect();
    let rewritten = |n: &str| {
        n == "[Content_Types].xml"
            || n.starts_with("word/document.xml")
            || n == "word/_rels/document.xml.rels"
            || n.starts_with("word/styles.xml")
            || n.starts_with("word/numbering.xml")
            || n.starts_with("word/header")
            || n.starts_with("word/footer")
            || n.starts_with("word/diagrams/data")
    };
    for (name, bytes) in &parts {
        assert!(
            rewritten(name) || sources.contains(bytes),
            "{name} differs from every input part"
        );
    }
    let base = entries(&fixture("a.docx"));
    for (name, bytes) in &base {
        if !rewritten(name) {
            let out = parts.iter().find(|e| &e.0 == name).unwrap();
            assert_eq!(&out.1, bytes, "{name}");
        }
    }
    let dir = temp_dir("twice");
    let paths: Vec<PathBuf> = names.iter().map(|n| fixture(n)).collect();
    merge(&paths, &dir.join("1.docx"), None).unwrap();
    merge(&paths, &dir.join("2.docx"), None).unwrap();
    assert_eq!(
        std::fs::read(dir.join("1.docx")).unwrap(),
        std::fs::read(dir.join("2.docx")).unwrap()
    );
}

#[test]
fn smartart_keeps_its_drawing() {
    let (_, parts) = merged("smartart", &["c.docx", "a.docx"]);
    let data = text(&parts, "word/diagrams/data1.xml");
    let id = data
        .split("relId=\"")
        .nth(1)
        .map(|s| &s[..s.find('"').unwrap()]);
    let rels = text(&parts, "word/_rels/document.xml.rels");
    let id = id.expect("the data names its drawing");
    let rel = rels
        .split("<Relationship ")
        .find(|r| r.contains(&format!("Id=\"{id}\"")))
        .unwrap();
    assert!(rel.contains("diagramDrawing") && rel.contains("diagrams/drawing1.xml"));
}
