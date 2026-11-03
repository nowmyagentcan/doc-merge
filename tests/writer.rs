mod common;

use std::io::Write;
use std::path::Path;

use common::*;
use doc_merge::Run;
use doc_merge::error::Warnings;
use doc_merge::package::Package;
use doc_merge::writer::{self, Body, Plan};
use quick_xml::events::{BytesStart, BytesText, Event};

const PNG: &str = "image/png";

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
    let size = writer::write(plan, &mut inputs, &out).unwrap();
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
        writer::write(plan, &mut inputs, &out).unwrap();
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
    writer::write(plan, &mut inputs, &out).unwrap();
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
    writer::write(plan, &mut inputs, &out).unwrap();

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
        writer::write(plan, &mut inputs, &out).unwrap();
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
        let e = writer::write(plan, &mut inputs, &out).unwrap_err();
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
    let e = writer::write(plan, &mut inputs, &dir.join("out.docx")).unwrap_err();
    assert_eq!(e.code, "limit_exceeded");
    assert_eq!(
        e.message,
        "Part word/media/image1.png of the output is over the limit: 3,072 MiB (at most 2,047 MiB)."
    );
    assert_eq!(files_in(&dir), 1);
}
