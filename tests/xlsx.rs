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

#[test]
fn formulas_follow_the_renames_everywhere() {
    let (report, parts) = merged("formulas", &["a.xlsx", "b.xlsx"]);
    let kinds: Vec<(&str, &str)> = report
        .renames
        .iter()
        .map(|r| (r.kind, r.to.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("sheet", "Summary (2)"),
            ("table", "Sales_2"),
            ("defined_name", "Total_2")
        ]
    );
    let detail = text(&parts, "xl/worksheets/sheet4.xml");
    for f in [
        "<f>'Summary (2)'!B2</f>",
        "<f>SUM(Sales_2[Amount])</f>",
        "<f>Total_2*2</f>",
        "<f>SUM('Summary (2):Detail'!A1)</f>",
        "<f>Local+1</f>",
        "<f>Sales_2[[#This Row],[Amount]]*2</f>",
        "<f>SUBTOTAL(109,Sales_2[Double])</f>",
        "_xlfn._xlws.FILTER(Sales_2[Item],Sales_2[Amount]&gt;2)</f>",
        "<xm:f>B4&gt;'Summary (2)'!$A$1/5</xm:f>",
        "<xm:f>'Summary (2)'!$A$1:$A$3</xm:f>",
        "<xm:f>Detail!B4:B6</xm:f>",
        r#"location="'Summary (2)'!A1""#,
    ] {
        assert!(detail.contains(f), "{f} in {detail}");
    }
    let workbook = text(&parts, "xl/workbook.xml");
    let names = elements(&workbook, "definedName");
    assert!(names.contains(&"Detail!$B$4".to_string()), "{names:?}");
    assert!(workbook.contains(r#"<definedName name="Total_2">Detail!$B$4</definedName>"#));
    assert!(workbook.contains(r#"<definedName name="Total">Summary!$B$2</definedName>"#));
    assert!(workbook.contains(r#"<definedName name="Local" localSheetId="3">'Summary (2)'!$A$2<"#));
    assert!(workbook.contains(
        r#"<definedName name="_xlnm.Print_Area" localSheetId="2">'Summary (2)'!$A$1:$B$3<"#
    ));
    let table = text(&parts, "xl/tables/table2.xml");
    assert!(table.contains(r#" id="2""#), "{table}");
    assert!(table.contains(r#"name="Sales_2" displayName="Sales_2""#));
    assert!(table.contains(
        "<calculatedColumnFormula>Sales_2[[#This Row],[Amount]]*2</calculatedColumnFormula>"
    ));
}

#[test]
fn external_links_are_added_after_the_output_s() {
    let (_, parts) = merged("links", &["b.xlsx", "b.xlsx"]);
    let workbook = text(&parts, "xl/workbook.xml");
    assert_eq!(workbook.matches("<externalReference ").count(), 2);
    let first = text(&parts, "xl/worksheets/sheet2.xml");
    assert!(first.contains("<f>[1]Sheet1!$A$1</f>"));
    let second = text(&parts, "xl/worksheets/sheet4.xml");
    assert!(second.contains("<f>[2]Sheet1!$A$1</f>"));
    assert!(
        parts
            .iter()
            .any(|p| p.0 == "xl/externalLinks/externalLink2.xml")
    );
}

/// A workbook with sheet `A` of `cells` and `definedNames` content.
fn named_book(cells: &str, names: &str) -> Vec<(String, Vec<u8>)> {
    let mut parts = book(&[("A", cells), ("Other", "")], None);
    let workbook = text(&parts, "xl/workbook.xml").replace(
        "</sheets>",
        &format!("</sheets><definedNames>{names}</definedNames>"),
    );
    set(&mut parts, "xl/workbook.xml", workbook.as_bytes());
    parts
}

#[test]
fn equal_names_are_shared_and_others_renamed() {
    let dir = temp_dir("share");
    let a = named_book(
        "",
        r#"<definedName name="Rate">0.2</definedName><definedName name="Top">A!$A$1</definedName><definedName name="Uses">Rate*2</definedName>"#,
    );
    let b = named_book(
        r#"<row r="1"><c r="A1"><f>Rate+Top+Uses+Only+Local</f></c></row>"#,
        r#"<definedName name="rate">0.2</definedName><definedName name="Top">A!$A$1</definedName><definedName name="Only">1</definedName><definedName name="Local" localSheetId="0">Rate</definedName><definedName name="Uses">Rate*2</definedName>"#,
    );
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (report, parts) = merged_files("share-out", &paths);
    let workbook = text(&parts, "xl/workbook.xml");
    // Top now reads 'A (2)'!$A$1, so it differs; Rate and Uses stay equal and are shared.
    assert_eq!(workbook.matches("name=\"Top").count(), 2);
    assert!(workbook.contains(r#"<definedName name="Top_2">'A (2)'!$A$1</definedName>"#));
    assert!(workbook.contains(r#"<definedName name="Only">1</definedName>"#));
    assert!(workbook.contains(r#"<definedName name="Local" localSheetId="2">Rate</definedName>"#));
    assert_eq!(workbook.to_lowercase().matches("name=\"rate\"").count(), 1);
    assert_eq!(workbook.matches("name=\"Uses\"").count(), 1);
    let sheet = text(&parts, "xl/worksheets/sheet3.xml");
    assert!(
        sheet.contains("<f>Rate+Top_2+Uses+Only+Local</f>"),
        "{sheet}"
    );
    let names: Vec<&str> = report
        .renames
        .iter()
        .filter(|r| r.kind == "defined_name")
        .map(|r| r.to.as_str())
        .collect();
    assert_eq!(names, ["Top_2"]);
}

#[test]
fn a_name_stops_being_equal_when_a_name_it_uses_is_renamed() {
    let dir = temp_dir("chain");
    let a = named_book(
        "",
        r#"<definedName name="One">Two</definedName><definedName name="Two">Three</definedName><definedName name="Three">1</definedName>"#,
    );
    let b = named_book(
        "",
        r#"<definedName name="One">Two</definedName><definedName name="Two">Three</definedName><definedName name="Three">2</definedName>"#,
    );
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (_, parts) = merged_files("chain-out", &paths);
    let workbook = text(&parts, "xl/workbook.xml");
    assert!(workbook.contains(r#"<definedName name="Three_2">2</definedName>"#));
    assert!(workbook.contains(r#"<definedName name="Two_2">Three_2</definedName>"#));
    assert!(workbook.contains(r#"<definedName name="One_2">Two_2</definedName>"#));
}

#[test]
fn a_sheet_scope_name_hides_a_renamed_workbook_name() {
    let dir = temp_dir("hidden");
    let a = named_book("", r#"<definedName name="Total">1</definedName>"#);
    let b = book(
        &[
            ("B1", r#"<row r="1"><c r="A1"><f>Total</f></c></row>"#),
            ("B2", r#"<row r="1"><c r="A1"><f>Total</f></c></row>"#),
        ],
        None,
    );
    let mut b = b;
    let workbook = text(&b, "xl/workbook.xml").replace(
        "</sheets>",
        r#"</sheets><definedNames><definedName name="Total">2</definedName><definedName name="Total" localSheetId="1">3</definedName></definedNames>"#,
    );
    set(&mut b, "xl/workbook.xml", workbook.as_bytes());
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (_, parts) = merged_files("hidden-out", &paths);
    assert!(text(&parts, "xl/worksheets/sheet3.xml").contains("<f>Total_2</f>"));
    assert!(text(&parts, "xl/worksheets/sheet4.xml").contains("<f>Total</f>"));
}

/// A table part `name` (id 1) on the first sheet of a workbook.
fn with_table(parts: &mut Vec<(String, Vec<u8>)>, name: &str) {
    let table = format!(
        r#"<table {SML_NS} id="1" name="{name}" displayName="{name}" ref="A1:A2"><autoFilter ref="A1:A2"/><tableColumns count="1"><tableColumn id="1" name="X"/></tableColumns></table>"#
    );
    set(&mut *parts, "xl/tables/table1.xml", table.as_bytes());
    let types = text(parts, "[Content_Types].xml").replace(
        "</Types>",
        &format!(
            r#"<Override PartName="/xl/tables/table1.xml" ContentType="{SML}table+xml"/></Types>"#
        ),
    );
    set(parts, "[Content_Types].xml", types.as_bytes());
    let table_rel = format!("{REL}table");
    let rels = rels(&[("rId1", table_rel.as_str(), "../tables/table1.xml")]);
    set(
        parts,
        "xl/worksheets/_rels/sheet1.xml.rels",
        rels.as_bytes(),
    );
    let sheet = text(parts, "xl/worksheets/sheet1.xml").replace(
        "</worksheet>",
        r#"<tableParts count="1"><tablePart r:id="rId1"/></tableParts></worksheet>"#,
    );
    set(parts, "xl/worksheets/sheet1.xml", sheet.as_bytes());
}

#[test]
fn tables_and_names_share_one_namespace() {
    let dir = temp_dir("namespace");
    let mut a = named_book("", r#"<definedName name="Rates">1</definedName>"#);
    with_table(&mut a, "People");
    // b's table clashes with a's name, b's name with a's table.
    let mut b = named_book("", r#"<definedName name="People">People+1</definedName>"#);
    with_table(&mut b, "Rates");
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (report, parts) = merged_files("namespace-out", &paths);
    let renamed: Vec<(&str, &str)> = report
        .renames
        .iter()
        .filter(|r| r.kind != "sheet")
        .map(|r| (r.kind, r.to.as_str()))
        .collect();
    assert_eq!(
        renamed,
        [("table", "Rates_2"), ("defined_name", "People_2")]
    );
    let table = text(&parts, "xl/tables/table2.xml");
    assert!(
        table.contains(r#"id="2" name="Rates_2" displayName="Rates_2""#),
        "{table}"
    );
    let workbook = text(&parts, "xl/workbook.xml");
    assert!(workbook.contains(r#"<definedName name="People_2">People_2+1</definedName>"#));
}

#[test]
fn a_new_name_avoids_the_names_of_its_own_workbook() {
    let dir = temp_dir("own");
    let a = named_book("", r#"<definedName name="N">1</definedName>"#);
    let b = named_book(
        "",
        r#"<definedName name="N">2</definedName><definedName name="N_2">3</definedName>"#,
    );
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (_, parts) = merged_files("own-out", &paths);
    let workbook = text(&parts, "xl/workbook.xml");
    assert!(workbook.contains(r#"<definedName name="N_3">2</definedName>"#));
    assert!(workbook.contains(r#"<definedName name="N_2">3</definedName>"#));
}

#[test]
fn a_formula_that_can_t_be_read_is_refused_by_cell() {
    let dir = temp_dir("unreadable");
    let a = book(&[("A", "")], None);
    let b = book(
        &[("B", r#"<row r="4"><c r="C4"><f>SUM("open</f></c></row>"#)],
        None,
    );
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let e = merge(&paths, &dir.join("out.xlsx"), &Options::default(), None).unwrap_err();
    assert_eq!(e.code, "unsupported_variant");
    assert_eq!(
        e.message,
        "'b.xlsx' has a formula it can't read (B!C4), which doc-merge can't merge yet."
    );
    assert_eq!(e.input, Some(2));
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        2,
        "nothing written"
    );
}
