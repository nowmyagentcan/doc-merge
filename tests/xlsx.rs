mod common;

use std::path::{Path, PathBuf};

use common::*;
use doc_merge::xlsx::a1::tokens;
use doc_merge::{Options, Report, merge};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The text of every `name` element in an XML part, with the common references resolved.
fn elements(xml: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    xml.split(&open)
        .skip(1)
        .filter(|s| s.starts_with(['>', ' ']))
        .filter_map(|s| {
            let end = s.find('>')?;
            let body = s[end + 1..].split(&close).next()?;
            (!s[..end].ends_with('/')).then(|| {
                body.replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&amp;", "&")
            })
        })
        .collect()
}

#[test]
fn every_formula_in_the_fixtures_tokenizes_back_to_itself() {
    let mut seen = 0;
    for name in ["a.xlsx", "b.xlsx", "c.xlsx", "p.xlsx"] {
        for (part, bytes) in entries(&fixture(name)) {
            if !part.ends_with(".xml") {
                continue;
            }
            let xml = String::from_utf8_lossy(&bytes);
            for element in ["f", "xm:f", "definedName", "calculatedColumnFormula", "c:f"] {
                for f in elements(&xml, element) {
                    let t = tokens(&f).unwrap_or_else(|| panic!("{name} {part}: {f}"));
                    assert_eq!(t.iter().map(|t| t.text).collect::<String>(), f);
                    seen += 1;
                }
            }
        }
    }
    assert!(seen > 25, "only {seen} formulas");
}

/// Merges files into a fresh folder, twice; returns the report and the output's parts.
fn merged_files(test: &str, paths: &[PathBuf]) -> (Report, Vec<(String, Vec<u8>)>) {
    let dir = temp_dir(test);
    let out = dir.join("out.xlsx");
    let report = merge(paths, &out, &Options::default(), None).unwrap();
    let parts = entries(&out);
    let again = dir.join("again.xlsx");
    merge(paths, &again, &Options::default(), None).unwrap();
    assert!(
        std::fs::read(&out).unwrap() == std::fs::read(&again).unwrap(),
        "two runs differ"
    );
    let _ = std::fs::remove_dir_all(dir);
    (report, parts)
}

fn merged(test: &str, names: &[&str]) -> (Report, Vec<(String, Vec<u8>)>) {
    let paths: Vec<PathBuf> = names.iter().map(|n| fixture(n)).collect();
    merged_files(test, &paths)
}

const SML_NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
const SML: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";

/// A workbook of sheets (name, `sheetData` content) and, when given, shared strings.
fn book(sheets: &[(&str, &str)], strings: Option<&str>) -> Vec<(String, Vec<u8>)> {
    let mut list = String::new();
    let mut relations = vec![];
    let mut extra = vec![];
    for (i, (name, data)) in sheets.iter().enumerate() {
        let n = i + 1;
        list += &format!(r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#);
        relations.push((
            format!("rId{n}"),
            format!("{REL}worksheet"),
            format!("worksheets/sheet{n}.xml"),
        ));
        extra.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!("{SML}worksheet+xml"),
            format!(r#"<worksheet {SML_NS}><sheetData>{data}</sheetData></worksheet>"#),
        ));
    }
    if let Some(s) = strings {
        relations.push((
            "rId90".into(),
            format!("{REL}sharedStrings"),
            "sharedStrings.xml".into(),
        ));
        extra.push((
            "xl/sharedStrings.xml".into(),
            format!("{SML}sharedStrings+xml"),
            format!(r#"<sst {SML_NS}>{s}</sst>"#),
        ));
    }
    let extra: Vec<(&str, &str, &str)> = extra
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect();
    let relations: Vec<(&str, &str, &str)> = relations
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect();
    let mut parts = minimal("xlsx", &extra);
    let workbook = format!(r#"<workbook {SML_NS}><sheets>{list}</sheets></workbook>"#);
    set(&mut parts, "xl/workbook.xml", workbook.as_bytes());
    set(
        &mut parts,
        "xl/_rels/workbook.xml.rels",
        rels(&relations).as_bytes(),
    );
    parts
}

fn book_file(dir: &Path, name: &str, parts: &[(String, Vec<u8>)]) -> PathBuf {
    write(dir, name, &build(parts))
}

fn sheet_names(report: &Report, input: usize) -> Vec<&str> {
    let sheets = report.inputs[input].sheets.as_ref().unwrap();
    sheets.iter().map(|s| s.output_name.as_str()).collect()
}

#[test]
fn sheets_follow_the_base_with_clashes_renamed() {
    let (report, parts) = merged("sheets", &["a.xlsx", "b.xlsx"]);
    assert_eq!(sheet_names(&report, 0), ["Data", "Summary"]);
    assert_eq!(sheet_names(&report, 1), ["Summary (2)", "Detail"]);
    assert_eq!(report.renames[0].kind, "sheet");
    assert_eq!(report.renames[0].to, "Summary (2)");
    let workbook = text(&parts, "xl/workbook.xml");
    let order: Vec<&str> = workbook
        .split("<sheet ")
        .skip(1)
        .map(|s| s.split('"').nth(1).unwrap())
        .collect();
    assert_eq!(order, ["Data", "Summary", "Summary (2)", "Detail"]);
    assert!(workbook.contains(r#"name="Summary (2)" sheetId="3""#));
    assert!(workbook.contains(r#"name="Detail" sheetId="4""#));
}

#[test]
fn shared_strings_are_shared_and_counted() {
    let dir = temp_dir("strings");
    let a = book(
        &[("One", r#"<row r="1"><c r="A1" t="s"><v>1</v></c></row>"#)],
        Some("<si><t>x</t></si><si><t>y</t></si><si><t>x</t></si>"),
    );
    let b = book(
        &[(
            "One",
            r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="s"> <v>2</v></c><c r="D1"><v>2</v></c></row>"#,
        )],
        Some("<si><t>new</t></si><si><t>y</t></si><si><r><t>rich</t></r></si>"),
    );
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (_, parts) = merged_files("strings-out", &paths);
    let strings = text(&parts, "xl/sharedStrings.xml");
    assert!(
        strings.ends_with("<si><t>x</t></si><si><t>new</t></si><si><r><t>rich</t></r></si></sst>"),
        "{strings}"
    );
    assert!(strings.contains(r#"count="6""#), "{strings}");
    assert!(strings.contains(r#"uniqueCount="5""#), "{strings}");
    let sheet = text(&parts, "xl/worksheets/sheet2.xml");
    assert!(sheet.contains(r#"<c r="A1" t="s"><v>3</v></c>"#), "{sheet}");
    assert!(sheet.contains(r#"<c r="B1" t="s"><v>1</v></c>"#), "{sheet}");
    assert!(
        sheet.contains(r#"<c r="C1" t="s"> <v>4</v></c>"#),
        "{sheet}"
    );
    assert!(sheet.contains(r#"<c r="D1"><v>2</v></c>"#), "{sheet}");
}

#[test]
fn a_base_without_shared_strings_gets_them() {
    let dir = temp_dir("no-strings");
    let a = book(&[("A", "")], None);
    let b = book(
        &[("B", r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#)],
        Some("<si><t>x</t></si>"),
    );
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (_, parts) = merged_files("no-strings-out", &paths);
    assert!(text(&parts, "xl/sharedStrings.xml").contains("<si><t>x</t></si>"));
    assert!(text(&parts, "[Content_Types].xml").contains("sharedStrings+xml"));
    assert!(text(&parts, "xl/_rels/workbook.xml.rels").contains("sharedStrings.xml"));
}

#[test]
fn sheet_names_stay_unique_and_short() {
    let dir = temp_dir("names");
    let long = "Quarterly results for the year9";
    assert_eq!(long.len(), 31);
    let a = book(&[(long, ""), ("Sheet1", ""), ("Sheet1 (2)", "")], None);
    let b = book(&[(long, ""), ("SHEET1", "")], None);
    let paths = [
        book_file(&dir, "a.xlsx", &a),
        book_file(&dir, "b.xlsx", &b),
        book_file(&dir, "c.xlsx", &b),
    ];
    let (report, _) = merged_files("names-out", &paths);
    assert_eq!(
        sheet_names(&report, 1),
        ["Quarterly results for the y (2)", "SHEET1 (3)"]
    );
    assert_eq!(
        sheet_names(&report, 2),
        ["Quarterly results for the y (3)", "SHEET1 (4)"]
    );
}

#[test]
fn appended_sheets_drop_their_selection_and_view() {
    let dir = temp_dir("views");
    let sheet = format!(
        r#"<worksheet {SML_NS}><sheetViews><sheetView tabSelected="1" workbookViewId="3"/></sheetViews><sheetData/></worksheet>"#
    );
    let mut b = book(&[("B", "")], None);
    set(&mut b, "xl/worksheets/sheet1.xml", sheet.as_bytes());
    let paths = [
        book_file(&dir, "a.xlsx", &book(&[("A", "")], None)),
        book_file(&dir, "b.xlsx", &b),
    ];
    let (_, parts) = merged_files("views-out", &paths);
    let out = text(&parts, "xl/worksheets/sheet2.xml");
    assert!(out.contains("<sheetView workbookViewId=\"0\"/>"), "{out}");
}
