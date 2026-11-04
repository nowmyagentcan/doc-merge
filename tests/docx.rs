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
            || n == "word/settings.xml"
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
    let styles_renamed = report.renames.iter().filter(|r| r.kind == "style");
    let renamed: Vec<_> = styles_renamed.clone().map(|r| r.to.as_str()).collect();
    assert!(renamed.contains(&"ClauseText (2)"), "{renamed:?}");
    assert!(styles_renamed.clone().all(|r| r.input == 2));
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
    assert!(report.renames.iter().all(|r| r.kind != "style"));
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
fn bookmark_ids_stay_unique_and_paired() {
    let (_, parts) = merged("bookmarks", &["a.docx", "b.docx", "c.docx", "a.docx"]);
    let doc = text(&parts, "word/document.xml");
    let mut starts = values(&doc, "w:bookmarkStart", "w:id");
    let mut ends = values(&doc, "w:bookmarkEnd", "w:id");
    starts.sort();
    ends.sort();
    assert_eq!(starts, ends);
    let unique: std::collections::HashSet<_> = starts.iter().collect();
    assert_eq!(unique.len(), starts.len(), "{starts:?}");
}

#[test]
fn only_one_style_is_the_default_of_its_type() {
    let (_, parts) = merged("one-default", &["a.docx", "b.docx"]);
    let styles = text(&parts, "word/styles.xml");
    let paragraph = r#"<w:style w:type="paragraph" w:default="1""#;
    assert_eq!(count(&styles, paragraph), 1);
}

/// Each section's references in a body, as (element, type, the part's text).
fn references(parts: &[(String, Vec<u8>)]) -> Vec<Vec<(String, String, String)>> {
    let doc = text(parts, "word/document.xml");
    let rels = text(parts, "word/_rels/document.xml.rels");
    let target = |id: &str| {
        let rel = rels
            .split("<Relationship ")
            .find(|r| r.contains(&format!("Id=\"{id}\"")))
            .unwrap_or_else(|| panic!("{id}"));
        let t = rel.split("Target=\"").nth(1).unwrap();
        t[..t.find('"').unwrap()].to_string()
    };
    doc.split("<w:sectPr")
        .skip(1)
        .map(|s| {
            let s = &s[..s.find("</w:sectPr>").unwrap_or(s.len())];
            let mut found = vec![];
            for element in ["headerReference", "footerReference"] {
                let types = values(s, &format!("w:{element}"), "w:type");
                let ids = values(s, &format!("w:{element}"), "r:id");
                for (t, id) in types.into_iter().zip(ids) {
                    let shown = words(&text(parts, &format!("word/{}", target(&id))));
                    found.push((element.to_string(), t, shown));
                }
            }
            found
        })
        .collect()
}

fn shown(section: &[(String, String, String)], element: &str, kind: &str) -> String {
    let found = section.iter().find(|r| r.0 == element && r.1 == kind);
    let found = found.unwrap_or_else(|| panic!("no {kind} {element}"));
    found.2.clone()
}

#[test]
fn every_document_keeps_its_own_headers_and_shows_nothing_of_the_one_before() {
    let (_, parts) = merged("headers", &["a.docx", "b.docx", "c.docx"]);
    let sections = references(&parts);
    let header = "headerReference";
    // a had even headers off: its even pages keep showing its default header.
    assert_eq!(shown(&sections[0], header, "default"), "A header");
    assert_eq!(shown(&sections[0], header, "even"), "A header");
    assert_eq!(shown(&sections[0], header, "first"), "A first page header");
    let page = shown(&sections[0], "footerReference", "default");
    assert_eq!(shown(&sections[0], "footerReference", "even"), page);
    assert_eq!(sections[0].len(), 6);
    assert!(sections[1].is_empty(), "a's second section inherits");
    assert_eq!(shown(&sections[2], header, "default"), "B odd header");
    assert_eq!(shown(&sections[2], header, "even"), "B even header");
    assert_eq!(sections[3].len(), 6);
    assert!(sections[3].iter().all(|r| r.2.is_empty()), "{sections:?}");
    assert!(text(&parts, "word/settings.xml").contains("<w:evenAndOddHeaders/>"));
    let types = text(&parts, "[Content_Types].xml");
    for (name, _) in parts.iter().filter(|p| p.0.starts_with("word/header")) {
        assert!(types.contains(&format!("/{name}\"")), "{name}");
    }
    let blank = r#"<w:pStyle w:val="Footer"/></w:pPr></w:p></w:ftr>"#;
    let footers = parts.iter().filter(|p| p.0.starts_with("word/footer"));
    assert!(
        footers
            .map(|p| String::from_utf8_lossy(&p.1))
            .any(|t| t.contains(blank))
    );
}

#[test]
fn even_headers_stay_off_when_no_document_has_them() {
    let (_, parts) = merged("headers-off", &["a.docx", "c.docx"]);
    let sections = references(&parts);
    assert_eq!(shown(&sections[0], "headerReference", "even"), "");
    assert_eq!(sections[2].len(), 6);
    let base = entries(&fixture("a.docx"));
    assert_eq!(
        text(&parts, "word/settings.xml"),
        text(&base, "word/settings.xml")
    );
}

const WORD_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const OFFICE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const WML: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.";

/// The parts of a Word file of `body` with `extra` parts (name, kind, xml), each related from
/// the main part as `rId{1…}`.
fn word_parts(body: &str, extra: &[(&str, &str, &str)]) -> Vec<(String, Vec<u8>)> {
    let doc = format!(
        r#"<w:document xmlns:w="{WORD_NS}" xmlns:r="{OFFICE_REL}" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml"><w:body>{body}</w:body></w:document>"#
    );
    let types: Vec<String> = extra.iter().map(|e| format!("{WML}{}+xml", e.1)).collect();
    let typed: Vec<(&str, &str, &str)> = extra
        .iter()
        .zip(&types)
        .map(|(e, t)| (e.0, t.as_str(), e.2))
        .collect();
    let mut parts = minimal("docx", &typed);
    set(&mut parts, "word/document.xml", doc.as_bytes());
    let list: Vec<(String, String, &str)> = extra
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let target = e.0.strip_prefix("word/").unwrap();
            (
                format!("rId{}", i + 1),
                format!("{OFFICE_REL}/{}", e.1),
                target,
            )
        })
        .collect();
    let list: Vec<(&str, &str, &str)> = list
        .iter()
        .map(|(i, k, t)| (i.as_str(), k.as_str(), *t))
        .collect();
    set(
        &mut parts,
        "word/_rels/document.xml.rels",
        rels(&list).as_bytes(),
    );
    parts
}

fn word_file(dir: &Path, name: &str, body: &str, extra: &[(&str, &str, &str)]) -> PathBuf {
    write(dir, name, &build(&word_parts(body, extra)))
}

fn merge_files(dir: &Path, files: &[PathBuf]) -> (Report, Vec<(String, Vec<u8>)>) {
    let out = dir.join("out.docx");
    let report = merge(files, &out, &Options::default(), None).unwrap();
    let parts = entries(&out);
    (report, parts)
}

#[test]
fn a_document_with_no_section_break_still_gets_its_slots() {
    let dir = temp_dir("no-sect");
    let a = word_file(&dir, "a.docx", "<w:p/>", &[]);
    let b = word_file(&dir, "b.docx", "<w:p/>", &[]);
    let (_, parts) = merge_files(&dir, &[a, b]);
    assert_eq!(references(&parts).last().unwrap().len(), 6);
}

#[test]
fn a_picture_in_a_header_keeps_its_relationship_in_the_copy_and_its_twin() {
    let dir = temp_dir("header-picture");
    let header = format!(
        r#"<w:hdr xmlns:w="{WORD_NS}" xmlns:r="{OFFICE_REL}"><w:p><w:r><w:pict r:id="rId2"/><w:t>H</w:t></w:r></w:p></w:hdr>"#
    );
    let body = r#"<w:p><w:r><w:pict r:id="rId2"/></w:r></w:p><w:sectPr><w:headerReference w:type="default" r:id="rId1"/></w:sectPr>"#;
    let mut parts = word_parts(body, &[("word/header1.xml", "header", &header)]);
    let image = format!("{OFFICE_REL}/image");
    let head = format!("{OFFICE_REL}/header");
    let main_rels = rels(&[
        ("rId1", head.as_str(), "header1.xml"),
        ("rId2", image.as_str(), "media/body.png"),
    ]);
    set(
        &mut parts,
        "word/_rels/document.xml.rels",
        main_rels.as_bytes(),
    );
    // The same id as the body picture's: it stays, as the header keeps its own relationships.
    let own = rels(&[("rId2", image.as_str(), "media/head.png")]);
    set(&mut parts, "word/_rels/header1.xml.rels", own.as_bytes());
    set(&mut parts, "word/media/body.png", b"body picture");
    set(&mut parts, "word/media/head.png", b"header picture");
    let b = write(&dir, "b.docx", &build(&parts));
    let a = word_file(&dir, "a.docx", "<w:p/>", &[]);
    let settings =
        format!(r#"<w:settings xmlns:w="{WORD_NS}"><w:evenAndOddHeaders/></w:settings>"#);
    let c = word_file(
        &dir,
        "c.docx",
        "<w:p/>",
        &[("word/settings.xml", "settings", &settings)],
    );
    let (_, parts) = merge_files(&dir, &[a, b, c]);
    let copies: Vec<_> = parts
        .iter()
        .filter(|p| String::from_utf8_lossy(&p.1).contains("<w:t>H</w:t>"))
        .collect();
    assert_eq!(copies.len(), 2, "the header and its twin");
    for (name, xml) in copies {
        assert!(String::from_utf8_lossy(xml).contains(r#"r:id="rId2""#));
        let own = text(&parts, &format!("word/_rels/{}.rels", &name[5..]));
        let target = own.split("Target=\"").nth(1).unwrap();
        let target = &target[..target.find('"').unwrap()];
        let picture = parts.iter().find(|p| p.0 == format!("word/{target}"));
        assert_eq!(picture.unwrap().1, b"header picture");
    }
}

#[test]
fn a_base_with_no_settings_gets_them_for_even_headers() {
    let dir = temp_dir("new-settings");
    let settings =
        format!(r#"<w:settings xmlns:w="{WORD_NS}"><w:evenAndOddHeaders w:val="1"/></w:settings>"#);
    let a = word_file(&dir, "a.docx", "<w:p/>", &[]);
    let b = word_file(
        &dir,
        "b.docx",
        "<w:p/>",
        &[("word/settings.xml", "settings", &settings)],
    );
    let (_, parts) = merge_files(&dir, &[a, b]);
    assert!(text(&parts, "word/settings.xml").contains("<w:evenAndOddHeaders/>"));
    assert!(text(&parts, "word/_rels/document.xml.rels").contains("settings.xml"));
}

#[test]
fn a_clashing_bookmark_is_renamed_with_what_points_to_it() {
    let (report, parts) = merged("bookmark-rename", &["a.docx", "b.docx"]);
    let doc = text(&parts, "word/document.xml");
    let names = values(&doc, "w:bookmarkStart", "w:name");
    assert!(names.contains(&"Intro".to_string()), "{names:?}");
    assert!(names.contains(&"Intro_2".to_string()), "{names:?}");
    let b = &doc[doc.find("Title of B").unwrap()..];
    assert!(
        b.contains(" REF Intro_2 "),
        "b's field follows its bookmark"
    );
    assert!(!b.contains(" REF Intro "));
    let renamed = report
        .renames
        .iter()
        .find(|r| r.kind == "bookmark")
        .unwrap();
    assert_eq!(
        (renamed.input, &*renamed.from, &*renamed.to),
        (2, "Intro", "Intro_2")
    );
    let a = &doc[..doc.find("Title of B").unwrap()];
    assert!(!a.contains("Intro_2"));
}

#[test]
fn the_same_file_twice_gets_toc_marks_of_its_own() {
    let (report, parts) = merged("twice", &["a.docx", "a.docx"]);
    let doc = text(&parts, "word/document.xml");
    let names = values(&doc, "w:bookmarkStart", "w:name");
    let unique: std::collections::HashSet<_> = names.iter().map(|n| n.to_lowercase()).collect();
    assert_eq!(unique.len(), names.len(), "{names:?}");
    let toc = report
        .renames
        .iter()
        .find(|r| r.from.starts_with("_Toc"))
        .unwrap();
    assert_eq!(toc.to, format!("{}_2", toc.from));
    assert!(doc.contains(&format!(" PAGEREF {} ", toc.to)));
}

/// A body of `n` paragraphs, each with a bookmark, a control, a drawing and a tracked insert.
fn busy_body(n: usize) -> String {
    (0..n)
        .map(|i| {
            format!(
                r#"<w:p w14:paraId="0000{i:04X}"><w:bookmarkStart w:id="{i}" w:name="Mark{i}"/><w:bookmarkEnd w:id="{i}"/><w:ins w:id="{i}"/><w:permStart w:id="{i}"/><w:permEnd w:id="{i}"/><w:sdt><w:sdtPr><w:id w:val="{i}"/></w:sdtPr></w:sdt><w:r><w:drawing><wp:inline><wp:docPr id="{i}" name="x"/></wp:inline></w:drawing></w:r></w:p>"#
            )
        })
        .collect()
}

#[test]
fn a_thousand_equal_bookmarks_and_every_pool_stay_unique() {
    let dir = temp_dir("pools");
    let body = busy_body(1000);
    let a = word_file(&dir, "a.docx", &body, &[]);
    let b = word_file(&dir, "b.docx", &body, &[]);
    let (report, parts) = merge_files(&dir, &[a, b]);
    let doc = text(&parts, "word/document.xml");
    for (element, attr, n) in [
        ("w:bookmarkStart", "w:id", 2000),
        ("w:bookmarkStart", "w:name", 2000),
        ("w:ins", "w:id", 2000),
        ("w:permStart", "w:id", 2000),
        ("w:id", "w:val", 2000),
        ("wp:docPr", "id", 2000),
    ] {
        let found = values(&doc, element, attr);
        let unique: std::collections::HashSet<_> = found.iter().collect();
        assert_eq!((found.len(), unique.len()), (n, n), "{element} {attr}");
    }
    let mut starts = values(&doc, "w:bookmarkStart", "w:id");
    let mut ends = values(&doc, "w:bookmarkEnd", "w:id");
    starts.sort();
    ends.sort();
    assert_eq!(starts, ends);
    assert_eq!(report.renames.len(), 1000);
    assert!(doc.contains(r#"w:name="Mark999_2""#));
    let paras = values(&doc, "w:p", "w14:paraId");
    let unique: std::collections::HashSet<_> = paras.iter().collect();
    assert_eq!(unique.len(), 2000, "repeated paragraph ids changed");
}

#[test]
fn a_drawing_id_at_the_top_leaves_room_below() {
    let dir = temp_dir("top-id");
    let drawing = |id: u32| {
        format!(
            r#"<w:p><w:r><w:drawing><wp:inline><wp:docPr id="{id}" name="x"/></wp:inline></w:drawing></w:r></w:p>"#
        )
    };
    let a = word_file(&dir, "a.docx", &drawing(2_147_483_646), &[]);
    let b = word_file(&dir, "b.docx", &drawing(7), &[]);
    let (_, parts) = merge_files(&dir, &[a, b]);
    let ids = values(&text(&parts, "word/document.xml"), "wp:docPr", "id");
    assert_eq!(ids, ["2147483646", "1"]);
}

#[test]
fn ids_in_the_bases_headers_and_notes_count() {
    let dir = temp_dir("story-ids");
    let header = format!(
        r#"<w:hdr xmlns:w="{WORD_NS}"><w:p><w:bookmarkStart w:id="40" w:name="Head"/><w:bookmarkEnd w:id="40"/></w:p></w:hdr>"#
    );
    let sect = r#"<w:sectPr><w:headerReference w:type="default" r:id="rId1"/></w:sectPr>"#;
    let a = word_file(
        &dir,
        "a.docx",
        &format!("<w:p/>{sect}"),
        &[("word/header1.xml", "header", &header)],
    );
    let b = word_file(
        &dir,
        "b.docx",
        r#"<w:p><w:bookmarkStart w:id="40" w:name="Head"/><w:bookmarkEnd w:id="40"/></w:p>"#,
        &[],
    );
    let (_, parts) = merge_files(&dir, &[a, b]);
    let doc = text(&parts, "word/document.xml");
    assert_eq!(values(&doc, "w:bookmarkStart", "w:id"), ["41"]);
    assert_eq!(values(&doc, "w:bookmarkStart", "w:name"), ["Head_2"]);
}

#[test]
fn the_last_edit_mark_of_an_appended_document_goes() {
    let dir = temp_dir("go-back");
    let mark = r#"<w:p><w:bookmarkStart w:id="1" w:name="_GoBack"/><w:bookmarkEnd w:id="1"/><w:bookmarkStart w:id="2" w:name="Stays"/><w:bookmarkEnd w:id="2"/></w:p>"#;
    let a = word_file(&dir, "a.docx", mark, &[]);
    let b = word_file(&dir, "b.docx", &mark.replace("Stays", "Other"), &[]);
    let (report, parts) = merge_files(&dir, &[a, b]);
    let doc = text(&parts, "word/document.xml");
    assert_eq!(
        values(&doc, "w:bookmarkStart", "w:name"),
        ["_GoBack", "Stays", "Other"]
    );
    assert_eq!(count(&doc, "<w:bookmarkEnd "), 3);
    assert!(report.renames.is_empty());
}

#[test]
fn links_and_split_fields_follow_a_renamed_bookmark() {
    let dir = temp_dir("field-kinds");
    let mark = r#"<w:bookmarkStart w:id="1" w:name="Note"/><w:bookmarkEnd w:id="1"/>"#;
    let body = format!(
        r#"<w:p>{mark}<w:hyperlink w:anchor="note"><w:r><w:t>x</w:t></w:r></w:hyperlink><w:fldSimple w:instr=" PAGEREF Note "/><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> RE</w:instrText></w:r><w:r><w:instrText>F No</w:instrText></w:r><w:r><w:instrText xml:space="preserve">te \h </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#
    );
    let a = word_file(&dir, "a.docx", mark_paragraph(mark).as_str(), &[]);
    let b = word_file(&dir, "b.docx", &body, &[]);
    let (_, parts) = merge_files(&dir, &[a, b]);
    let doc = text(&parts, "word/document.xml");
    assert!(doc.contains(r#"w:anchor="Note_2""#));
    assert!(doc.contains(r#"w:instr=" PAGEREF Note_2 ""#));
    assert!(doc.contains(r#"<w:instrText xml:space="preserve"> REF Note_2 \h </w:instrText>"#));
}

fn mark_paragraph(mark: &str) -> String {
    format!("<w:p>{mark}</w:p>")
}

#[test]
fn a_twin_shares_no_bookmark_or_drawing_id_with_its_part() {
    let dir = temp_dir("twin-ids");
    let header = format!(
        r#"<w:hdr xmlns:w="{WORD_NS}" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"><w:p><w:bookmarkStart w:id="1" w:name="Head"/><w:bookmarkEnd w:id="1"/><w:r><w:drawing><wp:inline><wp:docPr id="3" name="x"/></wp:inline></w:drawing></w:r></w:p></w:hdr>"#
    );
    let sect = r#"<w:sectPr><w:headerReference w:type="default" r:id="rId1"/></w:sectPr>"#;
    let a = word_file(
        &dir,
        "a.docx",
        &format!("<w:p/>{sect}"),
        &[("word/header1.xml", "header", &header)],
    );
    let settings =
        format!(r#"<w:settings xmlns:w="{WORD_NS}"><w:evenAndOddHeaders/></w:settings>"#);
    let b = word_file(
        &dir,
        "b.docx",
        "<w:p/>",
        &[("word/settings.xml", "settings", &settings)],
    );
    let (_, parts) = merge_files(&dir, &[a, b]);
    let mut drawings = vec![];
    let mut marks = 0;
    for (name, xml) in parts.iter().filter(|p| p.0.starts_with("word/header")) {
        let xml = String::from_utf8_lossy(xml);
        drawings.extend(values(&xml, "wp:docPr", "id"));
        marks += count(&xml, "<w:bookmarkStart ");
        assert_eq!(
            count(&xml, "<w:bookmarkStart "),
            count(&xml, "<w:bookmarkEnd "),
            "{name}"
        );
    }
    assert_eq!(drawings, ["3", "4"]);
    assert_eq!(marks, 1);
}
