mod common;

use std::path::{Path, PathBuf};

use common::*;
use doc_merge::xlsx::a1::tokens;
use doc_merge::{Options, Report};

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

/// Gives a workbook the style sheet with `lists` inside it.
fn with_styles(parts: &mut Vec<(String, Vec<u8>)>, lists: &str) {
    let sheet = format!(r#"<styleSheet {SML_NS}>{lists}</styleSheet>"#);
    set(parts, "xl/styles.xml", sheet.as_bytes());
    let types = text(parts, "[Content_Types].xml").replace(
        "</Types>",
        &format!(r#"<Override PartName="/xl/styles.xml" ContentType="{SML}styles+xml"/></Types>"#),
    );
    set(parts, "[Content_Types].xml", types.as_bytes());
    let rels = text(parts, "xl/_rels/workbook.xml.rels").replace(
        "</Relationships>",
        &format!(
            r#"<Relationship Id="rId80" Type="{REL}styles" Target="styles.xml"/></Relationships>"#
        ),
    );
    set(parts, "xl/_rels/workbook.xml.rels", rels.as_bytes());
}

const FONTS_A: &str = r#"<fonts count="2"><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="11"/><name val="Calibri"/></font></fonts>"#;
const FILLS: &str = r#"<fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>"#;
const BORDERS: &str =
    r#"<borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>"#;

/// Lists with a Calibri default and a custom format id 164 of code `code`.
fn plain_styles(code: &str, more: &str) -> String {
    format!(
        r#"<numFmts count="1"><numFmt numFmtId="164" formatCode="{code}"/></numFmts>{FONTS_A}{FILLS}{BORDERS}<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="3"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="164" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="14" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles>{more}"#
    )
}

fn styled_pair(test: &str, a: &str, b: &str, cells: &str) -> (Report, Vec<(String, Vec<u8>)>) {
    let dir = temp_dir(test);
    let mut first = book(&[("A", "")], None);
    with_styles(&mut first, a);
    let mut second = book(&[("B", cells)], None);
    with_styles(&mut second, b);
    let paths = [
        book_file(&dir, "a.xlsx", &first),
        book_file(&dir, "b.xlsx", &second),
    ];
    merged_files(&format!("{test}-out"), &paths)
}

#[test]
fn equal_styles_leave_the_style_sheet_alone() {
    let lists = plain_styles("0.0", "");
    let cells = r#"<row r="1" s="2" customFormat="1"><c r="A1" s="1"><v>1</v></c><c r="B1"><v>2</v></c></row>"#;
    let (_, parts) = styled_pair("equal-styles", &lists, &lists, cells);
    let sheet = text(&parts, "xl/styles.xml");
    assert_eq!(
        sheet,
        format!(r#"<styleSheet {SML_NS}>{lists}</styleSheet>"#)
    );
    assert!(text(&parts, "xl/worksheets/sheet2.xml").contains(cells));
}

#[test]
fn number_formats_with_a_taken_id_get_a_new_one() {
    let a = plain_styles("0.0", "");
    let b = plain_styles("0.00", "");
    let cells = r#"<row r="1"><c r="A1" s="1"><v>1</v></c><c r="B1" s="2"><v>2</v></c></row>"#;
    let (_, parts) = styled_pair("formats", &a, &b, cells);
    let sheet = text(&parts, "xl/styles.xml");
    assert!(sheet.contains(r#"<numFmts count="2">"#), "{sheet}");
    assert!(sheet.contains(r#"<numFmt numFmtId="165" formatCode="0.00"/>"#));
    assert!(sheet.contains(r#"<cellXfs count="4">"#));
    assert!(sheet.contains(r#"<xf numFmtId="165" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/></cellXfs>"#));
    let out = text(&parts, "xl/worksheets/sheet2.xml");
    // Format 1 is new; format 2 equals the base's and keeps its index.
    assert!(
        out.contains(r#"<c r="A1" s="3"><v>1</v></c><c r="B1" s="2">"#),
        "{out}"
    );
}

#[test]
fn a_cell_with_no_format_keeps_the_appended_default() {
    let a = plain_styles("0.0", "");
    let b = plain_styles("0.0", "").replace("Calibri", "Arial");
    let cells =
        r#"<row r="1" s="0" customFormat="1"><c r="A1"><v>1</v></c><c r="B1" t="s" s="0"/></row>"#;
    let (_, parts) = styled_pair("default", &a, &b, cells);
    let sheet = text(&parts, "xl/styles.xml");
    assert!(sheet.contains(r#"<fonts count="4">"#), "{sheet}");
    assert!(sheet.contains(r#"<cellXfs count="6">"#), "{sheet}");
    let out = text(&parts, "xl/worksheets/sheet2.xml");
    assert!(
        out.contains(r#"<row r="1" s="3" customFormat="1">"#),
        "{out}"
    );
    assert!(out.contains(r#"<c r="A1" s="3"><v>1</v></c>"#), "{out}");
    assert!(out.contains(r#"<c r="B1" t="s" s="3"/>"#), "{out}");
}

#[test]
fn named_styles_match_or_come_along() {
    let a = plain_styles("0.0", "").replace(
        r#"<cellStyle name="Normal" xfId="0" builtinId="0"/>"#,
        r#"<cellStyle name="Normal" xfId="0" builtinId="0"/><cellStyle name="Mine" xfId="0"/>"#,
    );
    let b = plain_styles("0.0", "")
        .replace("Calibri", "Arial")
        .replace(
            r#"<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>"#,
            r#"<cellStyleXfs count="3"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0"/><xf numFmtId="14" fontId="1" fillId="0" borderId="0"/></cellStyleXfs>"#,
        )
        .replace(
            r#"<cellStyle name="Normal" xfId="0" builtinId="0"/>"#,
            r#"<cellStyle name="Normal" xfId="0" builtinId="0"/><cellStyle name="MINE" xfId="1"/><cellStyle name="Theirs" xfId="2"/>"#,
        )
        .replace(
            r#"<xf numFmtId="14" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/>"#,
            r#"<xf numFmtId="14" fontId="1" fillId="0" borderId="0" xfId="2" applyFont="1"/>"#,
        );
    let (_, parts) = styled_pair("named", &a, &b, "");
    let sheet = text(&parts, "xl/styles.xml");
    // Normal and Mine map to the base's style format; Theirs comes along with its own.
    assert!(sheet.contains(r#"<cellStyleXfs count="2">"#), "{sheet}");
    assert!(
        sheet.contains(r#"<xf numFmtId="14" fontId="3" fillId="0" borderId="0"/></cellStyleXfs>"#)
    );
    assert!(sheet.contains(r#"<cellStyle name="Theirs" xfId="1"/></cellStyles>"#));
    assert!(sheet.contains(r#"<cellStyles count="3">"#));
    assert!(sheet.contains(
        r#"<xf numFmtId="14" fontId="3" fillId="0" borderId="0" xfId="1" applyFont="1"/></cellXfs>"#
    ));
}

#[test]
fn every_dxf_id_follows_and_table_styles_keep_their_look() {
    let dxf = |rgb: &str| {
        format!(r#"<dxf><fill><patternFill><bgColor rgb="{rgb}"/></patternFill></fill></dxf>"#)
    };
    let style = |id: u32| {
        format!(
            r#"<tableStyles count="1"><tableStyle name="Mine" count="1"><tableStyleElement type="wholeTable" dxfId="{id}"/></tableStyle></tableStyles>"#
        )
    };
    let a = plain_styles(
        "0.0",
        &format!(r#"<dxfs count="1">{}</dxfs>{}"#, dxf("FF111111"), style(0)),
    );
    let b = plain_styles(
        "0.0",
        &format!(
            r#"<dxfs count="3">{}{}{}</dxfs>{}"#,
            dxf("FF222222"),
            dxf("FF111111"),
            r#"<dxf><numFmt numFmtId="164" formatCode="0.000"/></dxf>"#,
            style(0)
        ),
    );
    let dir = temp_dir("dxf");
    let mut first = book(&[("A", "")], None);
    with_styles(&mut first, &a);
    let mut second = book(&[("B", "")], None);
    with_styles(&mut second, &b);
    with_table(&mut second, "T");
    let table = text(&second, "xl/tables/table1.xml")
        .replace(
            r#"ref="A1:A2">"#,
            r#"ref="A1:A2" headerRowDxfId="1" dataDxfId="0">"#,
        )
        .replace(r#"name="X"/>"#, r#"name="X" totalsRowDxfId="0"/>"#)
        .replace(
            "</table>",
            r#"<tableStyleInfo name="Mine" showRowStripes="1"/></table>"#,
        );
    set(&mut second, "xl/tables/table1.xml", table.as_bytes());
    let sheet = text(&second, "xl/worksheets/sheet1.xml").replace(
        "</sheetData>",
        r#"</sheetData><autoFilter ref="A1:A9"><sortState ref="A1:A9"><sortCondition ref="A1:A9" dxfId="1"/></sortState></autoFilter><conditionalFormatting sqref="A1"><cfRule type="expression" dxfId="0" priority="1"><formula>A1&gt;1</formula></cfRule></conditionalFormatting>"#,
    );
    set(&mut second, "xl/worksheets/sheet1.xml", sheet.as_bytes());
    let paths = [
        book_file(&dir, "a.xlsx", &first),
        book_file(&dir, "b.xlsx", &second),
    ];
    let (report, parts) = merged_files("dxf-out", &paths);
    let out = text(&parts, "xl/styles.xml");
    assert!(out.contains(r#"<dxfs count="3">"#), "{out}");
    let renamed = report
        .renames
        .iter()
        .find(|r| r.kind == "table_style")
        .unwrap();
    assert_eq!(
        (renamed.from.as_str(), renamed.to.as_str()),
        ("Mine", "Mine 2")
    );
    assert!(out.contains(r#"<tableStyle name="Mine 2" count="1"><tableStyleElement type="wholeTable" dxfId="1"/>"#), "{out}");
    assert!(out.contains(r#"<tableStyles count="2">"#));
    assert!(
        out.contains(r#"<numFmt numFmtId="165" formatCode="0.000"/></dxf>"#),
        "{out}"
    );
    let table = text(&parts, "xl/tables/table1.xml");
    assert!(
        table.contains(r#"headerRowDxfId="0" dataDxfId="1""#),
        "{table}"
    );
    assert!(table.contains(r#"totalsRowDxfId="1""#), "{table}");
    assert!(
        table.contains(r#"<tableStyleInfo name="Mine 2""#),
        "{table}"
    );
    let sheet = text(&parts, "xl/worksheets/sheet2.xml");
    assert!(
        sheet.contains(r#"<cfRule type="expression" dxfId="1" priority="1">"#),
        "{sheet}"
    );
    assert!(
        sheet.contains(r#"<sortCondition ref="A1:A9" dxfId="0"/>"#),
        "{sheet}"
    );
}

#[test]
fn a_base_without_a_style_sheet_gets_one() {
    let dir = temp_dir("no-styles");
    let mut b = book(&[("B", r#"<row r="1"><c r="A1" s="2"/></row>"#)], None);
    with_styles(&mut b, &plain_styles("0.0", ""));
    let paths = [
        book_file(&dir, "a.xlsx", &book(&[("A", "")], None)),
        book_file(&dir, "b.xlsx", &b),
    ];
    let (_, parts) = merged_files("no-styles-out", &paths);
    let sheet = text(&parts, "xl/styles.xml");
    assert!(sheet.contains(r#"<cellXfs count="3">"#), "{sheet}");
    let order: Vec<usize> = [
        "<numFmts",
        "<fonts",
        "<fills",
        "<borders",
        "<cellStyleXfs",
        "<cellXfs",
        "<cellStyles",
    ]
    .iter()
    .map(|l| sheet.find(l).unwrap())
    .collect();
    assert!(order.is_sorted(), "{sheet}");
    assert!(text(&parts, "xl/_rels/workbook.xml.rels").contains("styles.xml"));
    assert!(text(&parts, "xl/worksheets/sheet2.xml").contains(r#"s="2""#));
}

#[test]
fn too_many_formats_or_fonts_are_refused() {
    let formats = |from: usize| -> String {
        let xfs: String = (from..from + 40_000)
            .map(|i| format!(r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"><alignment indent="{i}"/></xf>"#))
            .collect();
        plain_styles("0.0", "").replace(
            r#"<cellXfs count="3">"#,
            &format!(r#"<cellXfs count="40003">{xfs}"#),
        )
    };
    let fonts = |from: usize| -> String {
        let list: String = (from..from + 300)
            .map(|i| format!(r#"<font><sz val="{i}"/><name val="Calibri"/></font>"#))
            .collect();
        plain_styles("0.0", "").replace(
            r#"<fonts count="2">"#,
            &format!(r#"<fonts count="302">{list}"#),
        )
    };
    for (a, b, words) in [
        (
            formats(0),
            formats(40_000),
            "cell format count is over the limit: 80,003 (at most 65,490)",
        ),
        (
            fonts(0),
            fonts(300),
            "font count is over the limit: 602 (at most 512)",
        ),
    ] {
        let dir = temp_dir("limits");
        let mut first = book(&[("A", "")], None);
        with_styles(&mut first, &a);
        let mut second = book(&[("B", "")], None);
        with_styles(&mut second, &b);
        let paths = [
            book_file(&dir, "a.xlsx", &first),
            book_file(&dir, "b.xlsx", &second),
        ];
        let e = merge(&paths, &dir.join("out.xlsx"), &Options::default(), None).unwrap_err();
        assert_eq!(e.code, "limit_exceeded");
        assert!(e.message.contains(words), "{}", e.message);
    }
}

#[test]
fn the_fixtures_keep_their_formats() {
    let (_, parts) = merged("styles", &["a.xlsx", "b.xlsx"]);
    let sheet = text(&parts, "xl/styles.xml");
    assert!(sheet.contains(r#"numFmtId="164" formatCode="0.00&quot; kg&quot;""#));
    assert!(sheet.contains("numFmtId=\"165\" formatCode=\"#,##0.0&quot; units&quot;\""));
    let count = |list: &str| {
        let open = format!("<{list} count=\"");
        let at = sheet.find(&open).unwrap() + open.len();
        let n = sheet[at..]
            .split('"')
            .next()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let end = format!("</{list}>");
        let body = sheet[at..].split(&end).next().unwrap();
        (n, body.to_string())
    };
    let (xfs, body) = count("cellXfs");
    assert_eq!(xfs, body.matches("<xf ").count());
    let (fonts, body) = count("fonts");
    assert_eq!(fonts, body.matches("<font>").count());
}

#[test]
fn another_theme_turns_into_literal_colours_and_fonts() {
    let (report, parts) = merged("theme", &["a.xlsx", "b.xlsx"]);
    let codes: Vec<&str> = report.warnings.list().iter().map(|w| w.code).collect();
    assert_eq!(codes, ["theme_differs"]);
    assert_eq!(report.warnings.list()[0].input, Some(2));
    let theme = text(&entries(&fixture("b.xlsx")), "xl/theme/theme1.xml");
    let srgb = |name: &str| {
        let at = theme.find(&format!("<a:{name}>")).unwrap();
        let v = theme[at..].split("srgbClr val=\"").nth(1).unwrap();
        v[..6].to_string()
    };
    let out = text(&parts, "xl/styles.xml");
    let (accent1, accent2, accent6) = (srgb("accent1"), srgb("accent2"), srgb("accent6"));
    assert!(
        out.contains(&format!(
            r#"<color rgb="FF{accent1}"/><name val="Trebuchet MS"/><family val="2"/></font>"#
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            r#"<color rgb="FF{accent6}" tint="-0.249977111117893"/>"#
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            r#"<fgColor rgb="FF{accent2}" tint="0.59999389629810485"/>"#
        )),
        "{out}"
    );
    let a = text(&entries(&fixture("a.xlsx")), "xl/styles.xml");
    // The base's fonts keep their links; the appended ones have none.
    assert_eq!(
        out.matches("<scheme ").count(),
        a.matches("<scheme ").count()
    );
    let first = text(&parts, "xl/worksheets/sheet3.xml");
    let accent3 = srgb("accent3");
    assert!(
        first.contains(&format!(r#"<tabColor rgb="FF{accent3}"/>"#)),
        "{first}"
    );
    let detail = text(&parts, "xl/worksheets/sheet4.xml");
    assert!(
        detail.contains(&format!(r#"<x14:colorSeries rgb="FF{accent2}"/>"#)),
        "{detail}"
    );
}

#[test]
fn the_same_theme_and_palette_keep_their_links() {
    let (report, parts) = merged("same-theme", &["a.xlsx", "a.xlsx"]);
    assert!(report.warnings.list().is_empty());
    let a = text(&entries(&fixture("a.xlsx")), "xl/styles.xml");
    assert_eq!(text(&parts, "xl/styles.xml"), a);
}

/// `theme` as a theme part of the workbook with its colours and fonts.
fn with_theme(parts: &mut Vec<(String, Vec<u8>)>, dark: &str, accent: &str, minor: &str) {
    let theme = format!(
        r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="T"><a:themeElements><a:clrScheme name="C"><a:dk1><a:sysClr val="windowText" lastClr="{dark}"/></a:dk1><a:lt1><a:srgbClr val="FEFEFE"/></a:lt1><a:dk2><a:srgbClr val="010203"/></a:dk2><a:lt2><a:srgbClr val="040506"/></a:lt2><a:accent1><a:srgbClr val="{accent}0001"/></a:accent1><a:accent2><a:srgbClr val="{accent}0002"/></a:accent2><a:accent3><a:srgbClr val="{accent}0003"/></a:accent3><a:accent4><a:srgbClr val="{accent}0004"/></a:accent4><a:accent5><a:srgbClr val="{accent}0005"/></a:accent5><a:accent6><a:srgbClr val="{accent}0006"/></a:accent6><a:hlink><a:srgbClr val="{accent}0007"/></a:hlink><a:folHlink><a:srgbClr val="{accent}0008"/></a:folHlink></a:clrScheme><a:fontScheme name="F"><a:majorFont><a:latin typeface="Major Face"/></a:majorFont><a:minorFont><a:latin typeface="{minor}"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#
    );
    set(parts, "xl/theme/theme1.xml", theme.as_bytes());
    let types = text(parts, "[Content_Types].xml").replace(
        "</Types>",
        r#"<Override PartName="/xl/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/></Types>"#,
    );
    set(parts, "[Content_Types].xml", types.as_bytes());
    let rels = text(parts, "xl/_rels/workbook.xml.rels").replace(
        "</Relationships>",
        &format!(r#"<Relationship Id="rId81" Type="{REL}theme" Target="theme/theme1.xml"/></Relationships>"#),
    );
    set(parts, "xl/_rels/workbook.xml.rels", rels.as_bytes());
}

#[test]
fn colours_resolve_by_theme_index_with_their_tint() {
    let dir = temp_dir("theme-index");
    let mut a = book(&[("A", "")], None);
    with_styles(&mut a, &plain_styles("0.0", ""));
    with_theme(&mut a, "000000", "BB", "Calibri");
    let colours: String = (0..14)
        .map(|i| {
            format!(
                r#"<font><sz val="{}"/><color theme="{i}" tint="0.5"/><name val="N"/></font>"#,
                10 + i
            )
        })
        .collect();
    let fonts = format!(
        r#"<fonts count="16"><font><sz val="9"/><name val="old"/><scheme val="minor"/></font><font><sz val="8"/><scheme val="major"/></font>{colours}</fonts>"#
    );
    let dxfs = r#"<dxfs count="1"><dxf><font><color theme="4"/></font><fill><gradientFill><stop position="0"><color theme="5"/></stop></gradientFill></fill></dxf></dxfs>"#;
    let lists = plain_styles("0.0", dxfs).replace(FONTS_A, &fonts);
    let mut b = book(&[("B", "")], None);
    with_styles(&mut b, &lists);
    with_theme(&mut b, "111111", "AA", "Minor Face");
    let sheet = format!(
        r#"<worksheet {SML_NS}><sheetPr><tabColor theme="9" tint="0.2"/></sheetPr><sheetData/><conditionalFormatting sqref="A1"><cfRule type="colorScale" priority="1"><colorScale><cfvo type="min"/><cfvo type="formula" val="B!$A$1"/><color theme="4"/><color rgb="FF000000"/></colorScale></cfRule></conditionalFormatting></worksheet>"#
    );
    set(&mut b, "xl/worksheets/sheet1.xml", sheet.as_bytes());
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (_, parts) = merged_files("theme-index-out", &paths);
    let out = text(&parts, "xl/styles.xml");
    assert!(
        out.contains(r#"<font><sz val="9"/><name val="Minor Face"/></font>"#),
        "{out}"
    );
    assert!(
        out.contains(r#"<font><sz val="8"/><name val="Major Face"/></font>"#),
        "{out}"
    );
    for (i, rgb) in [
        "111111", "AA0001", "AA0002", "AA0003", "AA0004", "AA0005", "AA0006", "AA0007", "AA0008",
    ]
    .iter()
    .zip([1, 4, 5, 6, 7, 8, 9, 10, 11])
    .map(|(rgb, i)| (i, rgb))
    {
        let want = format!(r#"<sz val="{}"/><color rgb="FF{rgb}" tint="0.5"/>"#, 10 + i);
        assert!(out.contains(&want), "{want} in {out}");
    }
    // Colours both themes share stay linked, so a table style can still override them.
    for i in [0, 2, 3] {
        let want = format!(r#"<sz val="{}"/><color theme="{i}" tint="0.5"/>"#, 10 + i);
        assert!(out.contains(&want), "{want} in {out}");
    }
    // Past the twelve there is nothing to resolve to.
    assert!(out.contains(r#"<color theme="12" tint="0.5"/>"#));
    assert!(
        out.contains(r#"<dxf><font><color rgb="FFAA0001"/></font>"#),
        "{out}"
    );
    assert!(
        out.contains(r#"<stop position="0"><color rgb="FFAA0002"/></stop>"#),
        "{out}"
    );
    let sheet = text(&parts, "xl/worksheets/sheet2.xml");
    assert!(
        sheet.contains(r#"<tabColor rgb="FFAA0006" tint="0.2"/>"#),
        "{sheet}"
    );
    assert!(
        sheet.contains(r#"<color rgb="FFAA0001"/><color rgb="FF000000"/>"#),
        "{sheet}"
    );
}

#[test]
fn a_custom_palette_resolves_on_either_side() {
    let fills = r#"<fills count="3"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill><fill><patternFill patternType="solid"><fgColor indexed="2"/><bgColor indexed="64"/></patternFill></fill></fills>"#;
    let palette = r#"<colors><indexedColors><rgbColor rgb="00123456"/><rgbColor rgb="00123457"/><rgbColor rgb="00ABCDEF"/></indexedColors></colors>"#;
    let plain = plain_styles("0.0", "").replace(FILLS, fills);
    let custom = plain_styles("0.0", palette).replace(FILLS, fills);
    let (_, parts) = styled_pair("palette-own", &plain, &custom, "");
    let out = text(&parts, "xl/styles.xml");
    assert!(
        out.contains(r#"<fgColor rgb="FFABCDEF"/><bgColor indexed="64"/>"#),
        "{out}"
    );
    let (_, parts) = styled_pair("palette-base", &custom, &plain, "");
    let out = text(&parts, "xl/styles.xml");
    assert!(
        out.contains(r#"<fgColor rgb="FFFF0000"/><bgColor indexed="64"/>"#),
        "{out}"
    );
    let (_, parts) = styled_pair("palette-same", &custom, &custom, "");
    let out = text(&parts, "xl/styles.xml");
    assert!(!out.contains(r#"rgb="FFABCDEF""#), "{out}");
}

#[test]
fn dynamic_array_metadata_comes_along_with_its_cells() {
    let (_, parts) = merged("metadata", &["a.xlsx", "b.xlsx"]);
    let b = entries(&fixture("b.xlsx"));
    assert_eq!(text(&parts, "xl/metadata.xml"), text(&b, "xl/metadata.xml"));
    assert!(text(&parts, "xl/_rels/workbook.xml.rels").contains("sheetMetadata"));
    assert!(text(&parts, "[Content_Types].xml").contains("sheetMetadata+xml"));
    assert!(text(&parts, "xl/worksheets/sheet4.xml").contains(r#"<c r="G1" t="str" cm="1""#));
}

#[test]
fn equal_metadata_blocks_stay_single_and_cells_move_up() {
    let (_, parts) = merged("metadata-twice", &["b.xlsx", "b.xlsx"]);
    let out = text(&parts, "xl/metadata.xml");
    assert_eq!(
        out.matches("<xda:dynamicArrayProperties").count(),
        1,
        "{out}"
    );
    assert!(
        out.contains(r#"<futureMetadata name="XLDAPR" count="1">"#),
        "{out}"
    );
    assert!(out.contains(r#"<cellMetadata count="2">"#), "{out}");
    assert_eq!(out.matches(r#"<rc t="1" v="0"/>"#).count(), 2, "{out}");
    assert!(text(&parts, "xl/worksheets/sheet4.xml").contains(r#"<c r="G1" t="str" cm="2""#));
}

/// Every output part is either one the merge rewrites or a byte-identical copy of an input's.
#[test]
fn untouched_parts_are_copied_byte_for_byte() {
    let (_, parts) = merged("fidelity", &["a.xlsx", "b.xlsx"]);
    let a = entries(&fixture("a.xlsx"));
    let b = entries(&fixture("b.xlsx"));
    let rewritten = |name: &str| {
        [
            "[Content_Types].xml",
            "xl/workbook.xml",
            "xl/_rels/workbook.xml.rels",
            "xl/styles.xml",
            "xl/sharedStrings.xml",
            "xl/worksheets/sheet3.xml",
            "xl/worksheets/sheet4.xml",
            "xl/tables/table2.xml",
            "xl/drawings/vmlDrawing2.vml",
            "xl/ctrlProps/ctrlProp1.xml",
        ]
        .contains(&name)
    };
    for (name, bytes) in &parts {
        let same = a.iter().chain(&b).any(|p| &p.1 == bytes);
        assert!(
            rewritten(name) || name.ends_with(".rels") || same,
            "{name} is not a copy"
        );
    }
    for (name, bytes) in &a {
        if !rewritten(name) && name != "xl/calcChain.xml" {
            assert_eq!(&text_or_bytes(&parts, name), bytes, "{name}");
        }
    }
    for name in [
        "xl/worksheets/sheet1.xml",
        "xl/worksheets/sheet2.xml",
        "xl/tables/table1.xml",
    ] {
        assert_eq!(text(&parts, name), text(&a, name), "{name}");
    }
}

fn text_or_bytes(parts: &[(String, Vec<u8>)], name: &str) -> Vec<u8> {
    let found = parts.iter().find(|p| p.0 == name);
    found.unwrap_or_else(|| panic!("no part {name}")).1.clone()
}

#[test]
fn prefixed_workbooks_keep_their_spelling() {
    let dir = temp_dir("prefixed");
    let spelled = |parts: &mut Vec<(String, Vec<u8>)>| {
        let main = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
        for name in [
            "xl/workbook.xml",
            "xl/worksheets/sheet1.xml",
            "xl/sharedStrings.xml",
        ] {
            let t = text(parts, name)
                .replace("</", "\0")
                .replace('<', "<x:")
                .replace('\0', "</x:")
                .replace(
                    &format!(r#"xmlns="{main}""#),
                    &format!(r#"xmlns:x="{main}""#),
                );
            set(parts, name, t.as_bytes());
        }
    };
    let mut a = book(&[("A", "")], Some("<si><t>x</t></si>"));
    spelled(&mut a);
    let b = book(
        &[("B", r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#)],
        Some("<si><t>y</t></si>"),
    );
    let paths = [book_file(&dir, "a.xlsx", &a), book_file(&dir, "b.xlsx", &b)];
    let (_, parts) = merged_files("prefixed-out", &paths);
    let strings = text(&parts, "xl/sharedStrings.xml");
    assert!(
        strings.contains("<x:si><x:t>x</x:t></x:si><x:si><x:t>y</x:t></x:si>"),
        "{strings}"
    );
    assert!(text(&parts, "xl/workbook.xml").contains(r#"<x:sheet name="B""#));
    let dir = temp_dir("prefixed-2");
    let paths = [book_file(&dir, "b.xlsx", &b), book_file(&dir, "a.xlsx", &a)];
    let (_, parts) = merged_files("prefixed-2-out", &paths);
    assert!(text(&parts, "xl/sharedStrings.xml").contains("<si><t>x</t></si>"));
    assert!(text(&parts, "xl/workbook.xml").contains(r#"<sheet name="A""#));
}

/// Gives sheet 1 a part: its relationship is rId7 of type `rel`, and `tag` goes in the sheet.
fn with_sheet_part(
    parts: &mut Vec<(String, Vec<u8>)>,
    name: &str,
    content_type: &str,
    rel: &str,
    body: &str,
    tag: &str,
) {
    set(parts, name, body.as_bytes());
    let types = text(parts, "[Content_Types].xml").replace(
        "</Types>",
        &format!(r#"<Override PartName="/{name}" ContentType="{content_type}"/></Types>"#),
    );
    set(parts, "[Content_Types].xml", types.as_bytes());
    let target = format!("../{}", name.trim_start_matches("xl/"));
    let list = rels(&[("rId7", rel, target.as_str())]);
    set(
        parts,
        "xl/worksheets/_rels/sheet1.xml.rels",
        list.as_bytes(),
    );
    let sheet =
        text(parts, "xl/worksheets/sheet1.xml").replace("</worksheet>", tag) + "</worksheet>";
    set(parts, "xl/worksheets/sheet1.xml", sheet.as_bytes());
}

fn with_vml(parts: &mut Vec<(String, Vec<u8>)>, vml: &str) {
    with_sheet_part(
        parts,
        "xl/drawings/vmlDrawing1.vml",
        "application/vnd.openxmlformats-officedocument.vmlDrawing",
        &format!("{REL}vmlDrawing"),
        vml,
        r#"<legacyDrawing r:id="rId7"/>"#,
    );
}

#[test]
fn charts_follow_their_renamed_sheets() {
    let (_, parts) = merged("chart", &["a.xlsx", "a.xlsx"]);
    let chart = text(&parts, "xl/charts/chart2.xml");
    assert!(chart.contains("<c:f>'Data (2)'!$B$1</c:f>"), "{chart}");
    assert!(chart.contains("<c:f>'Data (2)'!$A$2:$A$5</c:f>"), "{chart}");
    assert!(!chart.contains("<c:f>Data!"), "{chart}");
    let base = text(&parts, "xl/charts/chart1.xml");
    assert_eq!(
        base,
        text(&entries(&fixture("a.xlsx")), "xl/charts/chart1.xml")
    );
}

#[test]
fn shape_ids_clear_the_block_the_output_uses() {
    let (_, parts) = merged("shapes", &["b.xlsx", "b.xlsx"]);
    let vml = text(&parts, "xl/drawings/vmlDrawing2.vml");
    assert!(vml.contains(r#"<o:idmap v:ext="edit" data="2"/>"#), "{vml}");
    assert!(vml.contains(r#"id="_x0000_s2049""#), "{vml}");
    assert!(!vml.contains("_x0000_s1025"), "{vml}");
    assert!(vml.contains("_x0000_t201"), "{vml}");
    assert!(
        vml.contains("<x:FmlaLink>'Summary (2)'!$D$1</x:FmlaLink>"),
        "{vml}"
    );
    let sheet = text(&parts, "xl/worksheets/sheet4.xml");
    assert!(sheet.contains(r#"<control shapeId="2049""#), "{sheet}");
    let drawing = text(&parts, "xl/drawings/drawing2.xml");
    assert!(
        drawing.contains(r#"id="2049" name="Check Box 1""#),
        "{drawing}"
    );
    assert!(drawing.contains(r#"spid="_x0000_s2049""#), "{drawing}");
    let control = text(&parts, "xl/ctrlProps/ctrlProp2.xml");
    assert!(
        control.contains(r#"fmlaLink="'Summary (2)'!$D$1""#),
        "{control}"
    );
    let base = entries(&fixture("b.xlsx"));
    for part in ["xl/drawings/vmlDrawing1.vml", "xl/ctrlProps/ctrlProp1.xml"] {
        assert_eq!(text(&parts, part), text(&base, part), "{part}");
    }
}

#[test]
fn a_block_nobody_uses_stays() {
    let (_, parts) = merged("free-block", &["a.xlsx", "b.xlsx"]);
    let vml = text(&parts, "xl/drawings/vmlDrawing2.vml");
    assert!(
        vml.contains(r#"data="1""#) && vml.contains("_x0000_s1025"),
        "{vml}"
    );
    let sheet = text(&parts, "xl/worksheets/sheet4.xml");
    assert!(sheet.contains(r#"<control shapeId="1025""#), "{sheet}");
}

#[test]
fn vml_is_edited_as_text_and_blocks_move_one_by_one() {
    let dir = temp_dir("vml");
    let vml = r#"<xml xmlns:v="urn:schemas-microsoft-com:vml" xmlns:o="urn:schemas-microsoft-com:office:office" xmlns:x="urn:schemas-microsoft-com:office:excel"><o:shapedefaults v:ext="edit" spidmax="5122"/><o:shapelayout v:ext="edit"><o:idmap v:ext="edit" data="1,5"/></o:shapelayout><v:shape id="_x0000_s1025" o:spid="_x0000_s1025"><font>one<br>two</font><x:ClientData ObjectType="List"><x:FmlaRange>'Other&amp;'!$A$1:$A$2</x:FmlaRange><x:FmlaMacro>[0]!Sheet1</x:FmlaMacro></x:ClientData></v:shape><v:shape id="_x0000_s5121"/></xml>"#;
    let mut parts = book(&[("Other&amp;", "")], None);
    with_vml(&mut parts, vml);
    let base = book_file(&dir, "base.xlsx", &parts);
    let (_, parts) = merged_files("vml-out", &[base.clone(), base]);
    let out = text(&parts, "xl/drawings/vmlDrawing2.vml");
    assert!(out.contains(r#"spidmax="3074""#), "{out}");
    assert!(out.contains(r#"data="2,3""#), "{out}");
    assert!(
        out.contains(r#"id="_x0000_s2049" o:spid="_x0000_s2049""#),
        "{out}"
    );
    assert!(out.contains(r#"id="_x0000_s3073""#), "{out}");
    assert!(out.contains("one<br>two"), "{out}");
    assert!(
        out.contains("<x:FmlaRange>'Other&amp; (2)'!$A$1:$A$2</x:FmlaRange>"),
        "{out}"
    );
    assert!(
        out.contains("<x:FmlaMacro>[0]!Sheet1</x:FmlaMacro>"),
        "{out}"
    );
}

#[test]
fn shapes_and_control_cells_keep_their_links() {
    let dir = temp_dir("links");
    let drawing = r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"><xdr:sp macro="" textlink="Sheet1!$A$1"/></xdr:wsDr>"#;
    let mut parts = book(&[("Sheet1", "")], None);
    with_sheet_part(
        &mut parts,
        "xl/drawings/drawing1.xml",
        "application/vnd.openxmlformats-officedocument.drawing+xml",
        &format!("{REL}drawing"),
        drawing,
        r#"<drawing r:id="rId7"/><controls><control shapeId="1025" name="x"><controlPr linkedCell="Sheet1!$A$1" listFillRange="Sheet1!$B$1:$B$2"/></control></controls>"#,
    );
    let base = book_file(&dir, "base.xlsx", &parts);
    let (_, parts) = merged_files("links-out", &[base.clone(), base]);
    let out = text(&parts, "xl/drawings/drawing2.xml");
    assert!(out.contains(r#"textlink="'Sheet1 (2)'!$A$1""#), "{out}");
    let sheet = text(&parts, "xl/worksheets/sheet2.xml");
    assert!(
        sheet.contains(r#"linkedCell="'Sheet1 (2)'!$A$1""#),
        "{sheet}"
    );
    assert!(
        sheet.contains(r#"listFillRange="'Sheet1 (2)'!$B$1:$B$2""#),
        "{sheet}"
    );
}

#[test]
fn a_chart_formula_that_can_t_be_read_is_refused() {
    let dir = temp_dir("chart-formula");
    let chart = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:f>'Sheet1!$A$1</c:f></c:chartSpace>"#;
    let mut parts = book(&[("Sheet1", "")], None);
    with_sheet_part(
        &mut parts,
        "xl/charts/chart1.xml",
        "application/vnd.openxmlformats-officedocument.drawingml.chart+xml",
        &format!("{REL}chart"),
        chart,
        "",
    );
    let base = book_file(&dir, "base.xlsx", &parts);
    let out = dir.join("out.xlsx");
    let e = merge(&[base.clone(), base], &out, &Options::default(), None).unwrap_err();
    assert_eq!(e.code, "unsupported_variant");
    assert!(e.message.contains("xl/charts/chart1.xml"), "{}", e.message);
    assert!(!out.exists());
}

#[test]
fn the_calculation_chain_is_left_out() {
    let (_, parts) = merged("calc-chain", &["a.xlsx", "b.xlsx"]);
    assert!(parts.iter().all(|p| !p.0.contains("calcChain")));
    for part in ["[Content_Types].xml", "xl/_rels/workbook.xml.rels"] {
        let out = text(&parts, part);
        assert!(!out.contains("calcChain"), "{out}");
    }
}

#[test]
fn custom_views_of_appended_workbooks_are_left_out() {
    let dir = temp_dir("views");
    let guid = "{12345678-1234-4234-8234-123456789012}";
    let views = format!(
        r#"<customWorkbookViews><customWorkbookView name="v" guid="{guid}" windowWidth="9" windowHeight="9" activeSheetId="1"/></customWorkbookViews>"#
    );
    let names = format!(
        r#"<definedNames><definedName name="Z_12345678_1234_4234_8234_123456789012_.wvu.PrintArea" hidden="1" localSheetId="0">Sheet1!$A$1</definedName><definedName name="Kept">Sheet1!$A$1</definedName></definedNames>{views}"#
    );
    let mut parts = book(&[("Sheet1", "")], None);
    let workbook =
        text(&parts, "xl/workbook.xml").replace("</sheets>", &format!("</sheets>{names}"));
    set(&mut parts, "xl/workbook.xml", workbook.as_bytes());
    let sheet = text(&parts, "xl/worksheets/sheet1.xml").replace(
        "</sheetData>",
        &format!(
            r#"</sheetData><customSheetViews><customSheetView guid="{guid}"><pane state="frozen"/><selection/></customSheetView></customSheetViews><pageMargins left="1" right="1" top="1" bottom="1" header="1" footer="1"/>"#
        ),
    );
    set(&mut parts, "xl/worksheets/sheet1.xml", sheet.as_bytes());
    let views = book_file(&dir, "views.xlsx", &parts);
    let plain = book_file(&dir, "plain.xlsx", &book(&[("Plain", "")], None));
    let (report, parts) = merged_files("views-out", &[plain.clone(), views.clone()]);
    let codes: Vec<(&str, Option<usize>)> = report
        .warnings
        .list()
        .iter()
        .map(|w| (w.code, w.input))
        .collect();
    assert_eq!(codes, [("custom_views_dropped", Some(2))]);
    let sheet = text(&parts, "xl/worksheets/sheet2.xml");
    assert!(!sheet.contains("customSheetView"), "{sheet}");
    assert!(sheet.contains("</sheetData><pageMargins"), "{sheet}");
    let workbook = text(&parts, "xl/workbook.xml");
    assert!(
        !workbook.contains("wvu") && workbook.contains(r#"name="Kept""#),
        "{workbook}"
    );
    assert!(!workbook.contains("customWorkbookView"), "{workbook}");

    let (report, parts) = merged_files("views-base", &[views, plain]);
    assert!(report.warnings.list().is_empty());
    let workbook = text(&parts, "xl/workbook.xml");
    assert!(workbook.contains("customWorkbookView"), "{workbook}");
    assert!(text(&parts, "xl/worksheets/sheet1.xml").contains("customSheetView"));
}

const TC_NS: &str =
    r#"xmlns="http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments""#;

/// A one-sheet workbook with a threaded comments part and a people part.
fn threaded_book(comments: &str, people: &str) -> Vec<(String, Vec<u8>)> {
    let mut parts = book(&[("Sheet1", "")], None);
    with_sheet_part(
        &mut parts,
        "xl/threadedComments/threadedComment1.xml",
        "application/vnd.ms-excel.threadedcomments+xml",
        "http://schemas.microsoft.com/office/2017/10/relationships/threadedComment",
        &format!("<ThreadedComments {TC_NS}>{comments}</ThreadedComments>"),
        "",
    );
    let body = format!("<personList {TC_NS}>{people}</personList>");
    set(&mut parts, "xl/persons/person.xml", body.as_bytes());
    let types = text(&parts, "[Content_Types].xml").replace(
        "</Types>",
        r#"<Override PartName="/xl/persons/person.xml" ContentType="application/vnd.ms-excel.person+xml"/></Types>"#,
    );
    set(&mut parts, "[Content_Types].xml", types.as_bytes());
    let list = text(&parts, "xl/_rels/workbook.xml.rels").replace(
        "</Relationships>",
        r#"<Relationship Id="rId80" Type="http://schemas.microsoft.com/office/2017/10/relationships/person" Target="persons/person.xml"/></Relationships>"#,
    );
    set(&mut parts, "xl/_rels/workbook.xml.rels", list.as_bytes());
    parts
}

fn attribute_values<'t>(xml: &'t str, name: &str) -> Vec<&'t str> {
    let needle = format!(r#" {name}=""#);
    let values = xml.split(&needle).skip(1);
    values.map(|v| v.split('"').next().unwrap()).collect()
}

#[test]
fn a_thread_id_never_repeats_and_its_note_follows() {
    let (_, parts) = merged("threads", &["a.xlsx", "a.xlsx"]);
    let old = "{20C5690B-56D9-4D68-ACC2-B4FD8C765770}";
    let copy = text(&parts, "xl/threadedComments/threadedComment2.xml");
    let id = attribute_values(&copy, "id")[0];
    assert_ne!(id, old);
    assert_eq!(id.len(), old.len());
    let people = text(&parts, "xl/persons/person.xml");
    assert!(people.contains(attribute_values(&copy, "personId")[0]));
    assert_eq!(people.matches("<person ").count(), 1, "{people}");
    let notes = text(&parts, "xl/comments2.xml");
    assert!(
        notes.contains(&format!("<author>tc={id}</author>")),
        "{notes}"
    );
    assert!(notes.contains(&format!(r#"xr:uid="{id}""#)), "{notes}");
    assert!(!notes.contains(old), "{notes}");
    assert!(
        notes.contains("{83544F41-E56B-4473-BDD1-C62981D4A83B}"),
        "{notes}"
    );
    let base = entries(&fixture("a.xlsx"));
    for part in [
        "xl/comments1.xml",
        "xl/threadedComments/threadedComment1.xml",
    ] {
        assert_eq!(text(&parts, part), text(&base, part), "{part}");
    }
}

#[test]
fn a_base_with_no_people_gets_them() {
    let (_, parts) = merged("people", &["b.xlsx", "a.xlsx"]);
    let a = entries(&fixture("a.xlsx"));
    assert_eq!(
        text(&parts, "xl/persons/person.xml"),
        text(&a, "xl/persons/person.xml")
    );
    let list = text(&parts, "xl/_rels/workbook.xml.rels");
    assert!(list.contains("2017/10/relationships/person"), "{list}");
    let types = text(&parts, "[Content_Types].xml");
    assert!(
        types.contains(r#"PartName="/xl/persons/person.xml""#),
        "{types}"
    );
    assert!(
        types.contains("application/vnd.ms-excel.person+xml"),
        "{types}"
    );
    let copy = text(&parts, "xl/threadedComments/threadedComment1.xml");
    assert_eq!(copy, text(&a, "xl/threadedComments/threadedComment1.xml"));
}

#[test]
fn people_merge_by_id_and_replies_keep_their_parent() {
    let dir = temp_dir("people-merge");
    let (one, two) = (
        "{11111111-1111-4111-8111-111111111111}",
        "{22222222-2222-4222-8222-222222222222}",
    );
    let thread = "{AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA}";
    let reply = "{BBBBBBBB-BBBB-4BBB-8BBB-BBBBBBBBBBBB}";
    let person = |id: &str, name: &str| {
        format!(r#"<person displayName="{name}" id="{id}" userId="{name}" providerId="None"/>"#)
    };
    let comment = format!(
        r#"<threadedComment ref="A1" personId="{one}" id="{thread}"><text>Hi</text></threadedComment><threadedComment ref="A1" personId="{two}" id="{reply}" parentId="{thread}"><text>Hi {{there}}</text><mentions><mention mentionpersonId="{one}" mentionId="{reply}" startIndex="0" length="2"/></mentions></threadedComment>"#
    );
    let base = threaded_book(&comment, &person(one, "First"));
    let other = threaded_book(&comment, &(person(one, "Renamed") + &person(two, "Second")));
    let paths = [
        book_file(&dir, "base.xlsx", &base),
        book_file(&dir, "other.xlsx", &other),
    ];
    let (_, parts) = merged_files("people-merge-out", &paths);
    let people = text(&parts, "xl/persons/person.xml");
    assert_eq!(
        attribute_values(&people, "displayName"),
        ["First", "Second"]
    );
    let copy = text(&parts, "xl/threadedComments/threadedComment2.xml");
    let ids = attribute_values(&copy, "id");
    assert!(!ids.contains(&thread) && !ids.contains(&reply), "{copy}");
    assert_eq!(attribute_values(&copy, "parentId"), [ids[0]]);
    assert_eq!(attribute_values(&copy, "mentionId"), [ids[1]]);
    assert_eq!(attribute_values(&copy, "personId"), [one, two]);
    assert!(copy.contains("Hi {there}"), "{copy}");
}

/// Adds a part with an override.
fn with_part(parts: &mut Vec<(String, Vec<u8>)>, name: &str, content_type: &str, body: &[u8]) {
    set(parts, name, body);
    let types = text(parts, "[Content_Types].xml").replace(
        "</Types>",
        &format!(r#"<Override PartName="/{name}" ContentType="{content_type}"/></Types>"#),
    );
    set(parts, "[Content_Types].xml", types.as_bytes());
}

/// Merges a plain workbook and `parts`, expects `parts` refused as `variant` with no output,
/// then the other way round, which must merge.
fn refused_only_when_appended(test: &str, parts: &[(String, Vec<u8>)], variant: &str) {
    let dir = temp_dir(test);
    let plain = book_file(&dir, "plain.xlsx", &book(&[("Plain", "")], None));
    let held = book_file(&dir, "held.xlsx", parts);
    let out = dir.join("out.xlsx");
    let inputs = [plain.clone(), held.clone()];
    let e = merge(&inputs, &out, &Options::default(), None).unwrap_err();
    assert_eq!(e.code, "unsupported_variant", "{test}");
    assert_eq!(e.input, Some(2), "{test}");
    let expected = format!("'held.xlsx' has {variant}, which doc-merge can't merge yet.");
    assert_eq!(e.message, expected);
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
    assert_eq!(left.len(), 2, "{test}: {left:?}");
    merge(&[held, plain], &out, &Options::default(), None).unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn each_refused_part_names_its_variant() {
    let ms = "application/vnd.ms-excel.";
    let list = [
        (
            "xl/pivotTables/pivotTable1.xml",
            format!("{SML}pivotTable+xml"),
            "pivot tables",
        ),
        (
            "xl/pivotCache/pivotCacheDefinition1.xml",
            format!("{SML}pivotCacheDefinition+xml"),
            "pivot tables",
        ),
        (
            "xl/elsewhere/p.xml",
            format!("{SML}pivotTable+xml"),
            "pivot tables",
        ),
        (
            "xl/slicers/slicer1.xml",
            format!("{ms}slicer+xml"),
            "slicers",
        ),
        (
            "xl/slicerCaches/slicerCache1.xml",
            format!("{ms}slicerCache+xml"),
            "slicers",
        ),
        (
            "xl/timelines/timeline1.xml",
            format!("{ms}timeline+xml"),
            "timelines",
        ),
        (
            "xl/timelineCaches/timelineCache1.xml",
            format!("{ms}timelineCache+xml"),
            "timelines",
        ),
        (
            "xl/model/item.data",
            "application/vnd.openxmlformats-officedocument.model+data".into(),
            "a data model",
        ),
        (
            "xl/connections.xml",
            format!("{SML}connections+xml"),
            "data connections",
        ),
        (
            "xl/queryTables/queryTable1.xml",
            format!("{SML}queryTable+xml"),
            "query tables",
        ),
        (
            "xl/richData/rdrichvalue.xml",
            format!("{ms}rdrichvalue+xml"),
            "images in cells",
        ),
        (
            "xl/revisions/revisionHeaders.xml",
            format!("{SML}revisionHeaders+xml"),
            "shared-workbook revision history",
        ),
        (
            "xl/dialogsheets/sheet1.xml",
            format!("{SML}dialogsheet+xml"),
            "dialog sheets",
        ),
    ];
    for (name, content_type, variant) in list {
        let mut parts = book(&[("Sheet1", "")], None);
        with_part(&mut parts, name, &content_type, b"<x/>");
        refused_only_when_appended(name, &parts, variant);
    }
}

#[test]
fn xml_maps_show_in_the_workbook_s_relationships() {
    let mut parts = book(&[("Sheet1", "")], None);
    set(&mut parts, "xl/xmlMaps.xml", b"<MapInfo/>");
    let list = text(&parts, "xl/_rels/workbook.xml.rels").replace(
        "</Relationships>",
        &format!(
            r#"<Relationship Id="rId80" Type="{REL}xmlMaps" Target="xmlMaps.xml"/></Relationships>"#
        ),
    );
    set(&mut parts, "xl/_rels/workbook.xml.rels", list.as_bytes());
    refused_only_when_appended("xml-maps", &parts, "XML maps");
}

#[test]
fn a_mashup_is_found_in_utf_16_custom_xml() {
    let mut body = vec![0xFF, 0xFE];
    let xml = r#"<?xml version="1.0" encoding="utf-16"?><DataMashup xmlns="http://schemas.microsoft.com/DataMashup">AAAA</DataMashup>"#;
    body.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    let mut parts = book(&[("Sheet1", "")], None);
    with_part(&mut parts, "customXml/item1.xml", "application/xml", &body);
    refused_only_when_appended("mashup", &parts, "Power Query queries");
}

#[test]
fn other_custom_xml_is_no_refusal() {
    let dir = temp_dir("custom-xml");
    let mut parts = book(&[("Sheet1", "")], None);
    let body = b"<?xml version=\"1.0\"?><!-- DataMashup --><root>DataMashup</root>";
    with_part(&mut parts, "customXml/item1.xml", "application/xml", body);
    with_part(
        &mut parts,
        "customXml/item2.xml",
        "application/xml",
        b"<broken",
    );
    let held = book_file(&dir, "held.xlsx", &parts);
    let plain = book_file(&dir, "plain.xlsx", &book(&[("Plain", "")], None));
    merged_files("custom-xml-out", &[plain, held]);
}

fn with_metadata(parts: &mut Vec<(String, Vec<u8>)>, types: &str) {
    let body = format!(
        r#"<metadata {SML_NS}><metadataTypes count="1">{types}</metadataTypes></metadata>"#
    );
    with_part(
        parts,
        "xl/metadata.xml",
        &format!("{SML}sheetMetadata+xml"),
        body.as_bytes(),
    );
    let list = text(parts, "xl/_rels/workbook.xml.rels").replace(
        "</Relationships>",
        &format!(r#"<Relationship Id="rId80" Type="{REL}sheetMetadata" Target="metadata.xml"/></Relationships>"#),
    );
    set(parts, "xl/_rels/workbook.xml.rels", list.as_bytes());
}

#[test]
fn only_dynamic_array_metadata_comes_along() {
    let mut parts = book(&[("Sheet1", "")], None);
    with_metadata(
        &mut parts,
        r#"<metadataType name="XLRICHVALUE" minSupportedVersion="120000"/>"#,
    );
    refused_only_when_appended(
        "metadata-type",
        &parts,
        "cell metadata other than dynamic arrays",
    );
}

#[test]
fn a_cell_with_value_metadata_is_an_image_in_a_cell() {
    let cell = r#"<row r="1"><c r="A1" t="e" vm="1"><v>#VALUE!</v></c></row>"#;
    let parts = book(&[("Sheet1", cell)], None);
    refused_only_when_appended("vm", &parts, "images in cells");
}

#[test]
fn the_fixtures_show_the_two_refusals() {
    let dir = temp_dir("refusals");
    let out = dir.join("out.xlsx");
    let run = |names: [&str; 2]| {
        let paths = names.map(fixture);
        merge(&paths, &out, &Options::default(), None).unwrap_err()
    };
    let e = run(["a.xlsx", "p.xlsx"]);
    assert_eq!((e.code, e.input), ("unsupported_variant", Some(2)));
    assert!(e.message.contains("has pivot tables"), "{}", e.message);
    assert!(e.hint.unwrap().contains("paste their values"));
    let e = run(["a.xlsx", "c.xlsx"]);
    assert_eq!((e.code, e.input), ("date_system_differs", Some(2)));
    assert_eq!(
        e.message,
        "'c.xlsx' uses the 1904 date system and 'a.xlsx' uses the 1900 one; its dates would shift by four years."
    );
    let e = run(["c.xlsx", "a.xlsx"]);
    assert_eq!((e.code, e.input), ("date_system_differs", Some(2)));
    assert!(e.message.starts_with("'a.xlsx' uses the 1900 date system"));
    assert!(!out.exists());
}

#[test]
fn a_pivot_in_the_base_stays_as_it_is() {
    let (_, parts) = merged("pivot-base", &["p.xlsx", "a.xlsx"]);
    let p = entries(&fixture("p.xlsx"));
    for part in p.iter().filter(|e| e.0.contains("pivot")) {
        assert_eq!(&text_or_bytes(&parts, &part.0), &part.1, "{}", part.0);
    }
}

#[test]
fn a_cell_with_metadata_its_workbook_lacks_is_damage() {
    let dir = temp_dir("stray-metadata");
    let cell = r#"<row r="1"><c r="A1" cm="1"><v>1</v></c></row>"#;
    let held = book_file(&dir, "held.xlsx", &book(&[("Sheet1", cell)], None));
    let plain = book_file(&dir, "plain.xlsx", &book(&[("Plain", "")], None));
    let out = dir.join("out.xlsx");
    let e = merge(&[plain, held], &out, &Options::default(), None).unwrap_err();
    assert_eq!((e.code, e.input), ("bad_package", Some(2)));
    assert!(
        e.message.contains("Sheet1!A1 uses metadata 1"),
        "{}",
        e.message
    );
    assert!(!out.exists());
}
