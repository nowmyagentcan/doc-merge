mod common;

use std::path::{Path, PathBuf};

use common::*;
use doc_merge::{Options, Report, StyleMode, merge};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Merges fixtures into a fresh folder; returns the report and the output's parts.
fn merged(test: &str, names: &[&str]) -> (Report, Vec<(String, Vec<u8>)>) {
    merged_with(test, names, StyleMode::Keep)
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
    assert!(at("orient=\"landscape\"") < at("The end of A."));
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
            || ["footnotes", "endnotes", "comments"].iter().any(|k| {
                n.starts_with(&format!("word/{k}.xml")) || n.starts_with(&format!("word/_rels/{k}"))
            })
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
    merge(&paths, &dir.join("1.docx"), &Options::default(), None).unwrap();
    merge(&paths, &dir.join("2.docx"), &Options::default(), None).unwrap();
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

fn merged_with(test: &str, names: &[&str], styles: StyleMode) -> (Report, Vec<(String, Vec<u8>)>) {
    let out = temp_dir(test).join("out.docx");
    let paths: Vec<PathBuf> = names.iter().map(|n| fixture(n)).collect();
    let report = merge(&paths, &out, &Options { styles }, None).unwrap();
    let parts = entries(&out);
    (report, parts)
}

#[test]
fn clashing_styles_are_copied_under_new_names() {
    let (report, parts) = merged("keep", &["a.docx", "b.docx"]);
    let styles = text(&parts, "word/styles.xml");
    assert!(styles.contains(r#"w:styleId="ClauseText_2""#));
    assert!(styles.contains(r#"<w:name w:val="ClauseText (2)"/>"#));
    assert!(styles.contains(r#"<w:name w:val="Heading 1 (2)"/>"#));
    let renamed: Vec<_> = report.renames.iter().map(|r| r.to.as_str()).collect();
    assert!(renamed.contains(&"ClauseText (2)"), "{renamed:?}");
    assert!(
        report
            .renames
            .iter()
            .all(|r| r.input == 2 && r.kind == "style")
    );
    let doc = text(&parts, "word/document.xml");
    let b = &doc[doc.find("Title of B").unwrap()..];
    assert!(b.contains(r#"<w:pStyle w:val="ClauseText_2"/>"#));
    assert!(!b.contains(r#"<w:pStyle w:val="ClauseText"/>"#));
    let a = &doc[..doc.find("Title of B").unwrap()];
    assert!(a.contains(r#"<w:pStyle w:val="ClauseText"/>"#));
    assert_eq!(count(&styles, "</w:styles>"), 1);
}

#[test]
fn first_mode_takes_the_first_documents_styles() {
    let (report, parts) = merged_with("first", &["a.docx", "b.docx"], StyleMode::First);
    assert_eq!(report.renames, []);
    let doc = text(&parts, "word/document.xml");
    let b = &doc[doc.find("Title of B").unwrap()..];
    assert!(b.contains(r#"<w:pStyle w:val="ClauseText"/>"#));
    assert!(!text(&parts, "word/styles.xml").contains("ClauseText_2"));
}

#[test]
fn a_document_keeps_its_defaults_and_theme_under_the_base() {
    let (report, parts) = merged("look", &["a.docx", "b.docx"]);
    let styles = text(&parts, "word/styles.xml");
    let normal = styles
        .split("<w:style ")
        .find(|s| s.contains(r#"w:styleId="Normal_2""#));
    let normal = normal.expect("b's Normal is copied");
    assert!(normal.contains(r#"<w:sz w:val="24"/>"#), "{normal}");
    assert!(normal.contains(r#"w:ascii="Aptos""#), "{normal}");
    assert!(normal.contains(r#"w:line="278""#), "{normal}");
    assert!(!normal.contains("w:asciiTheme"), "{normal}");
    let codes: Vec<_> = report.warnings.list().iter().map(|w| w.code).collect();
    assert_eq!(codes, ["theme_differs"]);
    // Every paragraph of b either names a style or gets b's copied Normal.
    let doc = text(&parts, "word/document.xml");
    let b = &doc[doc.find("Title of B").unwrap()..];
    assert!(b.contains(r#"<w:pStyle w:val="Normal_2"/>"#));
    assert!(!b.contains("w:themeColor"));
    let a = &doc[..doc.find("The end of A.").unwrap()];
    assert!(!a.contains("Normal_2"));
    // The theme stays the base's.
    assert_eq!(
        text(&parts, "word/theme/theme1.xml"),
        text(&entries(&fixture("a.docx")), "word/theme/theme1.xml")
    );
}

#[test]
fn first_mode_folds_nothing_and_keeps_theme_links() {
    let (report, parts) = merged_with("first-look", &["a.docx", "b.docx"], StyleMode::First);
    assert!(report.warnings.list().is_empty());
    assert!(!text(&parts, "word/styles.xml").contains("Normal_2"));
}

/// The values of an attribute on an element, in order.
fn values(xml: &str, element: &str, attr: &str) -> Vec<String> {
    xml.split(&format!("<{element} "))
        .skip(1)
        .filter_map(|s| {
            let tag = &s[..s.find('>')?];
            let v = tag.split(&format!("{attr}=\"")).nth(1)?;
            Some(v[..v.find('"')?].to_string())
        })
        .collect()
}

#[test]
fn each_documents_lists_restart_with_their_own_ids() {
    for mode in [StyleMode::Keep, StyleMode::First] {
        let (_, parts) = merged_with("lists", &["a.docx", "b.docx"], mode);
        let numbering = text(&parts, "word/numbering.xml");
        let nsids = values(&numbering, "w:nsid", "w:val");
        let unique: std::collections::HashSet<_> = nsids.iter().collect();
        assert_eq!(unique.len(), nsids.len(), "{nsids:?}");
        let ids = values(&numbering, "w:num", "w:numId");
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        let abstracts = values(&numbering, "w:abstractNum", "w:abstractNumId");
        let targets = values(&numbering, "w:abstractNumId", "w:val");
        assert!(targets.iter().all(|t| abstracts.contains(t)), "{targets:?}");
        // Schema order: every abstract before the first num.
        assert!(numbering.rfind("<w:abstractNum ").unwrap() < numbering.find("<w:num ").unwrap());
        let doc = text(&parts, "word/document.xml");
        let split = doc.find("The end of A.").unwrap();
        let (a, b) = doc.split_at(split);
        let a_nums = values(a, "w:numId", "w:val");
        let b_nums = values(b, "w:numId", "w:val");
        assert!(!b_nums.is_empty());
        assert!(
            b_nums
                .iter()
                .all(|n| !a_nums.contains(n) && ids.contains(n))
        );
    }
}

#[test]
fn a_base_with_no_numbering_gets_the_part() {
    let (_, parts) = merged("new-numbering", &["c.docx", "a.docx"]);
    let numbering = text(&parts, "word/numbering.xml");
    assert!(numbering.contains("<w:abstractNum "));
    assert!(text(&parts, "[Content_Types].xml").contains("/word/numbering.xml"));
    assert!(text(&parts, "word/_rels/document.xml.rels").contains("numbering.xml"));
}

#[test]
fn notes_and_comments_join_the_outputs_parts_with_new_ids() {
    let (_, parts) = merged("notes", &["a.docx", "b.docx"]);
    let doc = text(&parts, "word/document.xml");
    let comments = text(&parts, "word/comments.xml");
    let ids = values(&comments, "w:comment", "w:id");
    let authors = values(&comments, "w:comment", "w:author");
    assert_eq!(authors, ["Author A", "Author B"]);
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 2, "{ids:?}");
    for element in [
        "w:commentReference",
        "w:commentRangeStart",
        "w:commentRangeEnd",
    ] {
        assert_eq!(values(&doc, element, "w:id"), ids, "{element}");
    }
    let endnotes = text(&parts, "word/endnotes.xml");
    assert!(words(&endnotes).contains("An endnote of B."));
    let wanted = values(&doc, "w:endnoteReference", "w:id");
    assert_eq!(wanted.len(), 1);
    assert!(values(&endnotes, "w:endnote", "w:id").contains(&wanted[0]));
    // The base's separators stay, and only once.
    assert_eq!(count(&endnotes, r#"w:type="separator""#), 1);
}

#[test]
fn a_base_with_no_notes_takes_the_first_part_with_its_separators() {
    let (_, parts) = merged("new-notes", &["c.docx", "a.docx"]);
    let footnotes = text(&parts, "word/footnotes.xml");
    assert_eq!(count(&footnotes, r#"w:type="separator""#), 1);
    assert!(words(&footnotes).contains("A footnote with a picture"));
    let wanted = values(
        &text(&parts, "word/document.xml"),
        "w:footnoteReference",
        "w:id",
    );
    assert!(values(&footnotes, "w:footnote", "w:id").contains(&wanted[0]));
    // The picture inside the note moved to the note part's own relationships.
    let rels = text(&parts, "word/_rels/footnotes.xml.rels");
    assert!(rels.contains("media/image"), "{rels}");
    let types = text(&parts, "[Content_Types].xml");
    assert!(types.contains("/word/footnotes.xml") && types.contains("/word/comments.xml"));
    let document_rels = text(&parts, "word/_rels/document.xml.rels");
    assert!(document_rels.contains("footnotes.xml"));
}

#[test]
fn content_controls_keep_their_text_and_lose_their_binding() {
    let (_, parts) = merged("binding", &["a.docx", "b.docx"]);
    let doc = text(&parts, "word/document.xml");
    assert!(!doc.contains("dataBinding"));
    assert!(words(&doc).contains("Title of B"));
    let (_, parts) = merged("binding-base", &["b.docx", "a.docx"]);
    assert!(text(&parts, "word/document.xml").contains("dataBinding"));
}

#[test]
fn only_one_style_is_the_default_of_its_type() {
    let (_, parts) = merged("one-default", &["a.docx", "b.docx"]);
    let styles = text(&parts, "word/styles.xml");
    let paragraph = r#"<w:style w:type="paragraph" w:default="1""#;
    assert_eq!(count(&styles, paragraph), 1);
}
