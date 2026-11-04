mod common;

use std::io::Write;
use std::path::Path;

use common::*;
use doc_merge::Run;
use doc_merge::error::{Error, Warnings};
use doc_merge::package::{Package, Rel, resolve};
use doc_merge::paths::Place;
use doc_merge::writer::{self, Body, Plan};
use quick_xml::events::{BytesStart, BytesText, Event};

const PNG: &str = "image/png";

/// Writes `plan` to exactly `path` and returns the file's size.
fn write_to(plan: Plan, inputs: &mut [Package], path: &Path) -> Result<u64, Error> {
    let place = Place::File {
        path: path.into(),
        replace: true,
    };
    writer::write(plan, inputs, &place).map(|(size, _)| size)
}

/// A Word package with a picture, properties and a part the main part points at.
fn word_with_parts() -> Vec<(String, Vec<u8>)> {
    let doc = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p><w:r><w:drawing><a:blip xmlns:a="urn:a" r:embed="rId1"/></w:drawing></w:r></w:p></w:body></w:document>"#;
    let core = "application/vnd.openxmlformats-package.core-properties+xml";
    let mut parts = minimal(
        "docx",
        &[(
            "docProps/core.xml",
            core,
            "<cp:coreProperties xmlns:cp=\"urn:cp\"/>",
        )],
    );
    set(&mut parts, "word/document.xml", doc.as_bytes());
    let doc_rels = rels(&[("rId1", IMAGE_REL, "media/image1.png")]);
    set(
        &mut parts,
        "word/_rels/document.xml.rels",
        doc_rels.as_bytes(),
    );
    set(&mut parts, "word/media/image1.png", &[7u8; 3000]);
    parts
}

fn base_plan(path: &Path) -> (Plan, Vec<Package>, Warnings) {
    let pkg = Package::open(path, 1, &Run::new(None)).unwrap();
    let mut warnings = Warnings::default();
    let plan = Plan::base(&pkg, &mut warnings).unwrap();
    (plan, vec![pkg], warnings)
}

fn files_in(dir: &Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

#[test]
fn the_base_is_copied_as_stored_in_its_order() {
    let dir = temp_dir("base-copy");
    let mut bytes = b"prepended".to_vec();
    bytes.extend(build(&word_with_parts()));
    let a = write(&dir, "a.docx", &bytes);
    let (plan, mut inputs, warnings) = base_plan(&a);
    let out = dir.join("out.docx");
    let size = write_to(plan, &mut inputs, &out).unwrap();
    assert!(warnings.list().is_empty());

    let written = std::fs::read(&out).unwrap();
    assert_eq!(size, written.len() as u64);
    assert!(
        written.starts_with(b"PK\x03\x04"),
        "nothing before the first entry"
    );
    assert!(written.ends_with(&[0, 0]), "no zip comment");
    let source = stored(&a);
    let output = stored(&out);
    let names: Vec<&String> = output.iter().map(|e| &e.0).collect();
    assert_eq!(names, source.iter().map(|e| &e.0).collect::<Vec<_>>());
    assert_eq!(names[0], "[Content_Types].xml");
    for (o, s) in output.iter().zip(&source).skip(1) {
        assert_eq!(o, s, "{} was not copied as stored", o.0);
    }
    assert_eq!(
        output[0].2,
        source_time(),
        "rewritten parts keep their timestamp"
    );
    assert_eq!(files_in(&dir), 2, "no temp file is left");
}

#[test]
fn templates_and_shows_become_documents() {
    let dir = temp_dir("switch");
    for (kind, to) in [
        ("dotx", "docx"),
        ("potx", "pptx"),
        ("ppsx", "pptx"),
        ("xltx", "xlsx"),
        ("docx", "docx"),
    ] {
        let a = write(&dir, &format!("a.{kind}"), &build(&minimal(kind, &[])));
        let (plan, mut inputs, _) = base_plan(&a);
        let out = dir.join(format!("out-{kind}.{to}"));
        write_to(plan, &mut inputs, &out).unwrap();
        let reopened = Package::open(&out, 1, &Run::new(None)).unwrap();
        assert_eq!(format!("{:?}", reopened.kind).to_lowercase(), to, "{kind}");
        let ct = text(&entries(&out), "[Content_Types].xml");
        assert_eq!(ct.matches("PartName=\"/").count(), 1, "{ct}");
    }
}

#[test]
fn signatures_are_left_out() {
    let origin =
        "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";
    let signature =
        "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/signature";
    let mut parts = minimal(
        "docx",
        &[
            (
                "_xmlsignatures/origin.sigs",
                "application/vnd.openxmlformats-package.digital-signature-origin",
                "",
            ),
            (
                "_xmlsignatures/sig1.xml",
                "application/vnd.openxmlformats-package.digital-signature-xmlsignature+xml",
                "<Signature/>",
            ),
        ],
    );
    let root = rels(&[
        ("rId1", DOC_REL, "word/document.xml"),
        ("rId9", origin, "_xmlsignatures/origin.sigs"),
    ]);
    set(&mut parts, "_rels/.rels", root.as_bytes());
    let sigs = rels(&[("rId1", signature, "sig1.xml")]);
    set(
        &mut parts,
        "_xmlsignatures/_rels/origin.sigs.rels",
        sigs.as_bytes(),
    );
    let dir = temp_dir("signed-out");
    let a = write(&dir, "s.docx", &build(&parts));
    let (plan, mut inputs, warnings) = base_plan(&a);
    let out = dir.join("out.docx");
    write_to(plan, &mut inputs, &out).unwrap();
    let got = entries(&out);
    assert!(got.iter().all(|e| !e.0.starts_with("_xmlsignatures")));
    assert!(!text(&got, "_rels/.rels").contains("digital-signature"));
    assert!(!text(&got, "[Content_Types].xml").contains("_xmlsignatures"));
    assert_eq!(warnings.list()[0].code, "signature_removed");
    assert!(!Package::open(&out, 1, &Run::new(None)).unwrap().signed);
}

#[test]
fn new_and_rewritten_parts_are_deflated_with_their_dates() {
    let dir = temp_dir("new-parts");
    let a = write(&dir, "a.docx", &build(&word_with_parts()));
    let (mut plan, mut inputs, _) = base_plan(&a);
    let name = plan.claim("word/media/image1.png");
    assert_eq!(name, "word/media/image2.png");
    plan.type_part(&name, PNG, true);
    plan.add(name, Body::Data(vec![1, 2, 3]), None);
    let streamed = plan.claim("word/streamed.xml");
    plan.type_part(&streamed, "application/vnd.test+xml", false);
    let stream: writer::Stream = Box::new(|out: &mut dyn Write, _: &mut [Package]| {
        let mut w = quick_xml::Writer::new(out);
        w.write_event(Event::Empty(BytesStart::new("root")))
            .unwrap();
        for i in 0..20_000 {
            w.write_event(Event::Comment(BytesText::new(&i.to_string())))
                .unwrap();
        }
        Ok(())
    });
    plan.add(streamed, Body::Stream(stream), None);
    let doc = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><a:blip xmlns:a="urn:a" r:embed="rId1"/></w:body></w:document>"#;
    plan.replace("word/document.xml", Body::Data(doc.to_vec()));
    let out = dir.join("out.docx");
    write_to(plan, &mut inputs, &out).unwrap();

    let got = stored(&out);
    let time = |n: &str| got.iter().find(|e| e.0 == n).unwrap().2;
    assert_eq!(time("word/document.xml"), source_time());
    assert_eq!(time("word/media/image2.png"), zip::DateTime::DEFAULT);
    let all = entries(&out);
    assert_eq!(text(&all, "word/document.xml").as_bytes(), doc);
    let streamed = text(&all, "word/streamed.xml");
    assert!(streamed.starts_with("<root/><!--0--><!--1-->"));
    assert_eq!(streamed.matches("<!--").count(), 20_000);
    let ct = text(&all, "[Content_Types].xml");
    assert!(ct.contains(
        "<Override PartName=\"/word/streamed.xml\" ContentType=\"application/vnd.test+xml\"/>"
    ));
    assert!(!ct.contains("image2"), "png is typed by its default");
    assert_eq!(
        all.last().unwrap().0,
        "word/streamed.xml",
        "new parts come after the base's"
    );
}

#[test]
fn a_file_that_appears_meanwhile_is_never_replaced() {
    let dir = temp_dir("appears");
    let a = write(&dir, "a.docx", &build(&minimal("docx", &[])));
    let (plan, mut inputs, _) = base_plan(&a);
    let path = write(&dir, "out.docx", b"theirs");
    let place = Place::File {
        path: path.clone(),
        replace: false,
    };
    let e = writer::write(plan, &mut inputs, &place).unwrap_err();
    assert_eq!(e.code, "output_exists");
    assert_eq!(std::fs::read(&path).unwrap(), b"theirs");
    assert_eq!(files_in(&dir), 2, "no temp file is left");

    let free = Place::Free {
        dir: dir.clone(),
        ext: "docx",
    };
    write(&dir, "merged.docx", b"theirs");
    let (plan, mut inputs, _) = base_plan(&a);
    let (_, path) = writer::write(plan, &mut inputs, &free).unwrap();
    assert_eq!(path, dir.join("merged-2.docx"));
    assert_eq!(std::fs::read(dir.join("merged.docx")).unwrap(), b"theirs");
    assert_eq!(files_in(&dir), 4, "no temp file is left");

    for n in 3..=999 {
        write(&dir, &format!("merged-{n}.docx"), b"");
    }
    let (plan, mut inputs, _) = base_plan(&a);
    let e = writer::write(plan, &mut inputs, &free).unwrap_err();
    assert_eq!(e.code, "output_exists");
    assert!(e.message.contains("merged-999.docx"), "{}", e.message);
    assert_eq!(files_in(&dir), 1001, "no temp file is left");
}

#[test]
fn a_new_folder_is_made_for_a_free_name() {
    let dir = temp_dir("new-folder");
    let a = write(&dir, "a.docx", &build(&minimal("docx", &[])));
    let (plan, mut inputs, _) = base_plan(&a);
    let new = dir.join("x").join("y");
    let place = Place::Free {
        dir: new.clone(),
        ext: "docx",
    };
    let (_, path) = writer::write(plan, &mut inputs, &place).unwrap();
    assert_eq!(path, new.join("merged.docx"));
    assert!(path.is_file());
}

#[test]
fn two_runs_give_the_same_bytes() {
    let dir = temp_dir("twice");
    let a = write(&dir, "a.dotx", &build(&minimal("dotx", &[])));
    let mut outs = vec![];
    for i in 0..2 {
        let (mut plan, mut inputs, _) = base_plan(&a);
        let n = plan.claim("word/new.xml");
        plan.type_part(&n, "application/xml", true);
        plan.add(n, Body::Data(b"<a/>".to_vec()), None);
        let out = dir.join(format!("out{i}.docx"));
        write_to(plan, &mut inputs, &out).unwrap();
        outs.push(std::fs::read(out).unwrap());
    }
    assert_eq!(outs[0], outs[1]);
}

#[test]
fn inconsistent_plans_fail_as_internal_with_no_output() {
    let dir = temp_dir("inconsistent");
    let a = write(&dir, "a.docx", &build(&word_with_parts()));
    let gone = rels(&[("rId1", IMAGE_REL, "media/gone.png")]).into_bytes();
    let unresolved = br#"<w:document xmlns:w="urn:w" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:p r:id="rId7"/></w:document>"#;
    type Break = Box<dyn Fn(&mut Plan)>;
    let cases: Vec<(&str, Break)> = vec![
        (
            "points to a missing part",
            Box::new(move |p| p.replace("word/_rels/document.xml.rels", Body::Data(gone.clone()))),
        ),
        (
            "has no relationship",
            Box::new(|p| p.replace("word/document.xml", Body::Data(unresolved.to_vec()))),
        ),
        (
            "has no content type",
            Box::new(|p| p.add("word/x.unknown".into(), Body::Data(vec![]), None)),
        ),
        (
            "name a missing part",
            Box::new(|p| p.types.add_override("word/never.xml", "application/xml")),
        ),
        (
            "two parts are named",
            Box::new(|p| p.add("WORD/media/image1.png".into(), Body::Data(vec![]), None)),
        ),
    ];
    for (want, break_it) in cases {
        let (mut plan, mut inputs, _) = base_plan(&a);
        break_it(&mut plan);
        let out = dir.join("out.docx");
        let e = write_to(plan, &mut inputs, &out).unwrap_err();
        assert_eq!(e.code, "internal", "{want}");
        assert!(e.message.contains(want), "{want}: {}", e.message);
        assert!(!out.exists());
    }
    assert_eq!(files_in(&dir), 1, "no temp file is left");
}

#[test]
fn output_entries_stay_under_2_gib() {
    let dir = temp_dir("huge-entry");
    let mut list: Vec<Raw> = word_with_parts().iter().map(|(n, d)| raw(n, d)).collect();
    list.iter_mut()
        .find(|e| e.name == "word/media/image1.png")
        .unwrap()
        .size = Some(3 << 30);
    let a = write(&dir, "a.docx", &raw_zip(&list));
    let (plan, mut inputs, _) = base_plan(&a);
    let e = write_to(plan, &mut inputs, &dir.join("out.docx")).unwrap_err();
    assert_eq!(e.code, "limit_exceeded");
    assert_eq!(
        e.message,
        "Part word/media/image1.png of the output is over the limit: 3,072 MiB (at most 2,047 MiB)."
    );
    assert_eq!(files_in(&dir), 1);
}

const CHART: &str = "application/vnd.openxmlformats-officedocument.drawingml.chart+xml";
const COLORS: &str = "application/vnd.ms-office.chartcolorstyle+xml";
const CHART_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart";
const LINK_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";
const HANDLED: &str = "urn:test:handled";

/// A document whose main part reaches a graph: two pictures (one equal to the base's, one
/// with the same size and CRC-32 but other bytes), two charts sharing a colour part that
/// points back at the first chart, an embedded workbook, an external link and a diagram
/// whose relationships need no change.
fn appended() -> Vec<(String, Vec<u8>)> {
    let forged = {
        let mut b = vec![8u8; 2996];
        let tail = forge_crc(&b, crc32(&[7u8; 3000]));
        b.extend(tail);
        b
    };
    assert_eq!(crc32(&forged), crc32(&[7u8; 3000]));
    let xlsx = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    let data = "application/vnd.openxmlformats-officedocument.drawingml.diagramData+xml";
    let mut parts = minimal(
        "docx",
        &[
            (
                "word/charts/chart1.xml",
                CHART,
                "<c:chartSpace xmlns:c=\"urn:c\"/>",
            ),
            (
                "word/charts/chart2.xml",
                CHART,
                "<c:chartSpace xmlns:c=\"urn:c\"><c:x/></c:chartSpace>",
            ),
            (
                "word/charts/colors1.xml",
                COLORS,
                "<cs:colorStyle xmlns:cs=\"urn:cs\"/>",
            ),
            (
                "word/embeddings/Microsoft_Excel_Worksheet.xlsx",
                xlsx,
                "PK-not-really",
            ),
            (
                "word/diagrams/data1.xml",
                data,
                "<dgm:dataModel xmlns:dgm=\"urn:d\"/>",
            ),
        ],
    );
    let doc_rels = rels(&[
        ("rId1", IMAGE_REL, "media/image1.png"),
        ("rId2", IMAGE_REL, "media/image2.png"),
        ("rId3", CHART_REL, "charts/chart1.xml"),
        ("rId4", CHART_REL, "charts/chart2.xml"),
        ("rId5", LINK_REL, "!https://example.com/"),
        ("rId6", "urn:test:data", "diagrams/data1.xml"),
    ]);
    set(
        &mut parts,
        "word/_rels/document.xml.rels",
        doc_rels.as_bytes(),
    );
    set(&mut parts, "word/media/image1.png", &[7u8; 3000]);
    set(&mut parts, "word/media/image2.png", &forged);
    let chart1 = rels(&[
        (
            "rId1",
            "urn:test:package",
            "../embeddings/Microsoft_Excel_Worksheet.xlsx",
        ),
        ("rId2", "urn:test:colors", "colors1.xml"),
        ("rId3", HANDLED, "../document.xml"),
    ]);
    set(
        &mut parts,
        "word/charts/_rels/chart1.xml.rels",
        chart1.as_bytes(),
    );
    let chart2 = rels(&[
        ("rId1", "urn:test:colors", "colors1.xml"),
        ("rId3", IMAGE_REL, "../media/image1.png"),
    ]);
    set(
        &mut parts,
        "word/charts/_rels/chart2.xml.rels",
        chart2.as_bytes(),
    );
    let colors = rels(&[("rId1", "urn:test:back", "chart1.xml")]);
    set(
        &mut parts,
        "word/charts/_rels/colors1.xml.rels",
        colors.as_bytes(),
    );
    let data_rels = rels(&[("rId1", LINK_REL, "!https://example.com/d")]);
    set(
        &mut parts,
        "word/diagrams/_rels/data1.xml.rels",
        data_rels.as_bytes(),
    );
    parts
}

/// The base: a picture and a chart with its colours, so the copies need new names.
fn base_with_chart() -> Vec<(String, Vec<u8>)> {
    let mut parts = word_with_parts();
    let ct = content_types(&[
        ("word/document.xml", &kind_parts("docx").1),
        ("word/charts/chart1.xml", CHART),
        ("word/charts/colors1.xml", COLORS),
    ]);
    set(&mut parts, "[Content_Types].xml", ct.as_bytes());
    set(
        &mut parts,
        "word/charts/chart1.xml",
        b"<c:chartSpace xmlns:c=\"urn:c\"/>",
    );
    set(
        &mut parts,
        "word/charts/colors1.xml",
        b"<cs:colorStyle xmlns:cs=\"urn:cs\"/>",
    );
    let doc_rels = rels(&[
        ("rId1", IMAGE_REL, "media/image1.png"),
        ("rId2", CHART_REL, "charts/chart1.xml"),
    ]);
    set(
        &mut parts,
        "word/_rels/document.xml.rels",
        doc_rels.as_bytes(),
    );
    let chart = rels(&[("rId1", "urn:test:colors", "colors1.xml")]);
    set(
        &mut parts,
        "word/charts/_rels/chart1.xml.rels",
        chart.as_bytes(),
    );
    parts
}

/// Copies the graph of every internal relationship of b's main part and writes the output;
/// returns the output names in relationship order.
fn copy_appended(
    dir: &Path,
    b_parts: &[(String, Vec<u8>)],
    out: &Path,
) -> Result<Vec<String>, Error> {
    let a = write(dir, "a.docx", &build(&base_with_chart()));
    let b = write(dir, "b.docx", &build(b_parts));
    let (mut plan, mut inputs, _) = base_plan(&a);
    inputs.push(Package::open(&b, 2, &Run::new(None))?);
    let main_rels = inputs[1].rels_of("word/document.xml")?;
    let handled = |r: &Rel, _: &str| (r.kind == HANDLED).then(|| "word/document.xml".to_string());
    let mut names = vec![];
    for r in main_rels.iter().filter(|r| !r.external) {
        let target = resolve("word/document.xml", &r.target).unwrap();
        names.push(plan.copy_graph(&mut inputs, 1, &target, &handled)?);
    }
    write_to(plan, &mut inputs, out)?;
    Ok(names)
}

type Stored = Vec<(String, Vec<u8>, zip::DateTime)>;

fn stored_bytes(list: &Stored, name: &str) -> Vec<u8> {
    list.iter().find(|e| e.0 == name).unwrap().1.clone()
}

#[test]
fn part_graphs_are_copied_once_with_their_relationships() {
    let dir = temp_dir("graph");
    let out = dir.join("out.docx");
    let names = copy_appended(&dir, &appended(), &out).unwrap();
    assert_eq!(
        names,
        [
            "word/media/image1.png",
            "word/media/image2.png",
            "word/charts/chart2.xml",
            "word/charts/chart3.xml",
            "word/diagrams/data1.xml",
        ],
        "the equal picture is shared; the look-alike is not"
    );
    let all = entries(&out);
    let listed: Vec<&str> = all.iter().map(|e| e.0.as_str()).collect();
    let colours = listed.iter().filter(|n| n.contains("colors")).count();
    assert_eq!(
        colours, 3,
        "the base's colours, one copy and its rels: {listed:?}"
    );
    assert!(listed.contains(&"word/embeddings/Microsoft_Excel_Worksheet.xlsx"));
    assert_eq!(text(&all, "word/media/image2.png").len(), 3000);

    let chart2 = text(&all, "word/charts/_rels/chart2.xml.rels");
    for want in [
        r#"Id="rId1" Type="urn:test:package" Target="../embeddings/Microsoft_Excel_Worksheet.xlsx""#,
        r#"Id="rId2" Type="urn:test:colors" Target="colors2.xml""#,
        r#"Id="rId3" Type="urn:test:handled" Target="../document.xml""#,
    ] {
        assert!(chart2.contains(want), "{want} in {chart2}");
    }
    let chart3 = text(&all, "word/charts/_rels/chart3.xml.rels");
    assert!(chart3.contains(r#"Id="rId1" Type="urn:test:colors" Target="colors2.xml""#));
    assert!(chart3.contains(r#"Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png""#));
    let colors = text(&all, "word/charts/_rels/colors2.xml.rels");
    assert!(
        colors.contains(r#"Target="chart2.xml""#),
        "the cycle links back: {colors}"
    );

    // Parts and unchanged relationships are copied as stored.
    let (source, output) = (stored(&dir.join("b.docx")), stored(&out));
    assert_eq!(
        stored_bytes(&output, "word/charts/chart2.xml"),
        stored_bytes(&source, "word/charts/chart1.xml")
    );
    let data_rels = "word/diagrams/_rels/data1.xml.rels";
    assert_eq!(
        stored_bytes(&output, data_rels),
        stored_bytes(&source, data_rels)
    );
    assert!(
        text(&all, data_rels).contains(r#"Target="https://example.com/d" TargetMode="External""#)
    );

    let ct = text(&all, "[Content_Types].xml");
    assert!(ct.contains(&format!(
        r#"<Override PartName="/word/charts/chart3.xml" ContentType="{CHART}"/>"#
    )));
    assert!(ct.contains(r#"<Override PartName="/word/embeddings/Microsoft_Excel_Worksheet.xlsx""#));

    let dir2 = temp_dir("graph-again");
    let again = dir2.join("out.docx");
    copy_appended(&dir2, &appended(), &again).unwrap();
    let same = std::fs::read(&out).unwrap() == std::fs::read(&again).unwrap();
    assert!(same, "two runs give the same bytes");
}

#[test]
fn a_missing_target_is_refused() {
    let mut parts = appended();
    let broken = rels(&[("rId1", "urn:x", "colors9.xml")]);
    set(
        &mut parts,
        "word/charts/_rels/chart1.xml.rels",
        broken.as_bytes(),
    );
    let dir = temp_dir("graph-missing");
    let e = copy_appended(&dir, &parts, &dir.join("out.docx")).unwrap_err();
    assert_eq!(
        e.message,
        "'b.docx' is damaged or unsafe: word/charts/chart1.xml points to a missing part colors9.xml."
    );
    assert_eq!(e.input, Some(2));
}
