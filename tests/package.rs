mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use doc_merge::error::Error;
use doc_merge::package::{Kind, Package};
use doc_merge::{Run, open_inputs};

fn open(path: &Path) -> Result<Package, Error> {
    Package::open(path, 2, &Run::new(None))
}

/// Opens `bytes` as an input and returns the error, checking it comes within 200 ms (the
/// best of three tries, so a busy machine doesn't fail the test).
fn refused(test: &str, bytes: &[u8]) -> Error {
    let p = write(&temp_dir(test), "in.docx", bytes);
    let mut best = Duration::MAX;
    let mut error = None;
    for _ in 0..3 {
        let t = Instant::now();
        error = Some(open(&p).expect_err("opened"));
        best = best.min(t.elapsed());
    }
    assert!(best < Duration::from_millis(200), "{test} took {best:?}");
    let e = error.unwrap();
    assert_eq!(e.input, Some(2), "{test}");
    e
}

fn docx() -> Vec<(String, Vec<u8>)> {
    minimal("docx", &[])
}

#[test]
fn every_kind_is_read_from_the_package() {
    let dir = temp_dir("kinds");
    for (kind, want) in [
        ("docx", Kind::Docx),
        ("dotx", Kind::Dotx),
        ("pptx", Kind::Pptx),
        ("potx", Kind::Potx),
        ("ppsx", Kind::Ppsx),
        ("xlsx", Kind::Xlsx),
        ("xltx", Kind::Xltx),
    ] {
        // The extension says nothing: every file is named .bin.
        let p = write(&dir, &format!("{kind}.bin"), &build(&minimal(kind, &[])));
        let pkg = open(&p).unwrap();
        assert_eq!(pkg.kind, want);
        assert_eq!(pkg.main, kind_parts(kind).0);
        assert!(!pkg.signed);
    }
}

#[test]
fn not_office_files() {
    for (test, bytes) in [
        ("empty", b"".to_vec()),
        ("text", b"hello, this is text".to_vec()),
        ("pk-only", b"PK\x03\x04 truncated".to_vec()),
        ("plain-zip", zip(&[("notes.txt", b"hi")])),
    ] {
        let e = refused(test, &bytes);
        assert_eq!(e.code, "not_office_file", "{test}");
        assert_eq!(
            e.message,
            "'in.docx' isn't a Word, PowerPoint or Excel file."
        );
    }
    let e = Package::open(Path::new("no/such/file.docx"), 1, &Run::new(None)).unwrap_err();
    assert_eq!(e.code, "file_not_found");
}

#[test]
fn compound_files_are_legacy_or_encrypted() {
    let mut cfb = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    cfb.resize(4096, 0);
    let e = refused("cfb", &cfb);
    assert_eq!(e.code, "legacy_or_encrypted");
}

#[test]
fn foreign_formats_are_named() {
    let odf = zip(&[
        ("mimetype", b"application/vnd.oasis.opendocument.text"),
        ("content.xml", b"<a/>"),
    ]);
    let e = refused("odf", &odf);
    assert_eq!(
        (e.code, e.message.as_str()),
        ("unsupported_format", "'in.docx' is an OpenDocument file.")
    );

    let mut xlsb = minimal("xlsx", &[]);
    let ct = content_types(&[(
        "xl/workbook.xml",
        "application/vnd.ms-excel.sheet.binary.macroEnabled.main",
    )]);
    set(&mut xlsb, "[Content_Types].xml", ct.as_bytes());
    let e = refused("xlsb", &build(&xlsb));
    assert_eq!(e.message, "'in.docx' is an Excel binary workbook (.xlsb).");

    let mut visio = docx();
    let ct = content_types(&[(
        "word/document.xml",
        "application/vnd.ms-visio.drawing.main+xml",
    )]);
    set(&mut visio, "[Content_Types].xml", ct.as_bytes());
    let e = refused("visio", &build(&visio));
    assert_eq!(
        e.message,
        "'in.docx' is a zip file but not an Office document."
    );

    let mut no_main = docx();
    set(
        &mut no_main,
        "_rels/.rels",
        rels(&[("rId1", "urn:other", "word/document.xml")]).as_bytes(),
    );
    assert_eq!(
        refused("no-main-rel", &build(&no_main)).code,
        "unsupported_format"
    );
}

#[test]
fn strict_and_macro_files_are_refused() {
    let mut strict = docx();
    let rel = "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument";
    set(
        &mut strict,
        "_rels/.rels",
        rels(&[("rId1", rel, "word/document.xml")]).as_bytes(),
    );
    assert_eq!(refused("strict", &build(&strict)).code, "strict_ooxml");

    let mut docm = docx();
    let ct = content_types(&[(
        "word/document.xml",
        "application/vnd.ms-word.document.macroEnabled.main+xml",
    )]);
    set(&mut docm, "[Content_Types].xml", ct.as_bytes());
    assert_eq!(refused("docm", &build(&docm)).code, "macro_enabled");

    let vba = minimal(
        "docx",
        &[(
            "word/vbaProject.bin",
            "application/vnd.ms-office.vbaProject",
            "x",
        )],
    );
    assert_eq!(refused("vba", &build(&vba)).code, "macro_enabled");

    let sheet = minimal(
        "xlsx",
        &[(
            "xl/macrosheets/sheet1.xml",
            "application/vnd.ms-excel.macrosheet+xml",
            "<a/>",
        )],
    );
    let e = refused("macrosheet", &build(&sheet));
    assert_eq!(
        e.message,
        "'in.docx' contains macros, which doc-merge doesn't merge."
    );
}

#[test]
fn opc_parts_must_be_there() {
    let mut no_ct = docx();
    no_ct.remove(0);
    let e = refused("no-ct", &build(&no_ct));
    assert_eq!(
        e.message,
        "'in.docx' is damaged or unsafe: [Content_Types].xml is missing."
    );

    let mut no_main = docx();
    no_main.retain(|p| p.0 != "word/document.xml");
    let e = refused("no-main", &build(&no_main));
    assert_eq!(
        e.message,
        "'in.docx' is damaged or unsafe: the main part is missing."
    );

    let mut no_rels = docx();
    no_rels.retain(|p| p.0 != "_rels/.rels");
    assert_eq!(refused("no-rels", &build(&no_rels)).code, "bad_package");

    let mut untyped = docx();
    set(&mut untyped, "word/thing.bin", b"x");
    let e = refused("untyped", &build(&untyped));
    assert_eq!(
        e.message,
        "'in.docx' is damaged or unsafe: part word/thing.bin has no content type."
    );

    let mut doctype = docx();
    let ct = content_types(&[]).replace("<Types", "<!DOCTYPE t [<!ENTITY x \"x\">]><Types");
    set(&mut doctype, "[Content_Types].xml", ct.as_bytes());
    let e = refused("doctype", &build(&doctype));
    assert_eq!(
        e.message,
        "'in.docx' is damaged or unsafe: part [Content_Types].xml contains a DOCTYPE."
    );
}

#[test]
fn unsafe_zips_are_refused() {
    let base = docx();
    let entries = |extra: Vec<Raw>| {
        let mut list: Vec<Raw> = base.iter().map(|(n, d)| raw(n, d)).collect();
        list.extend(extra);
        raw_zip(&list)
    };
    assert!(open(&write(&temp_dir("raw-ok"), "a.docx", &entries(vec![]))).is_ok());

    let reason = |test: &str, bytes: Vec<u8>| {
        let e = refused(test, &bytes);
        assert_eq!(e.code, "bad_package", "{test}: {}", e.message);
        e.message
            .trim_start_matches("'in.docx' is damaged or unsafe: ")
            .to_string()
    };
    let dup = entries(vec![raw("Word/Document.XML", b"<a/>")]);
    assert_eq!(reason("dup", dup), "two parts are named Word/Document.XML.");
    for name in [
        "../evil.xml",
        "/abs.xml",
        "C:x.xml",
        "a\\b.xml",
        "a//b.xml",
        "x.",
    ] {
        let r = reason("name", entries(vec![raw(name, b"<a/>")]));
        assert_eq!(r, format!("part name {name} is not allowed."));
    }
    let mut enc = raw("word/x.png", b"x");
    enc.flags = 1;
    assert_eq!(
        reason("enc", entries(vec![enc])),
        "part word/x.png is encrypted."
    );
    let mut bzip = raw("word/x.png", b"x");
    bzip.method = 12;
    assert_eq!(
        reason("method", entries(vec![bzip])),
        "part word/x.png uses compression method 12."
    );

    // A second entry hidden inside the first one's data.
    let inner = raw("word/b.png", b"bbbb");
    let mut hidden = local_header(&inner);
    hidden.extend(&inner.data);
    let outer = raw("word/a.png", &hidden);
    let mut bytes = vec![];
    let mut cd = vec![];
    for (n, d) in &base {
        let e = raw(n, d);
        cd.extend(central_header(&e, bytes.len() as u32));
        bytes.extend(local_header(&e));
        bytes.extend(&e.data);
    }
    cd.extend(central_header(&outer, bytes.len() as u32));
    let inner_at = bytes.len() + local_header(&outer).len();
    cd.extend(central_header(&inner, inner_at as u32));
    bytes.extend(local_header(&outer));
    bytes.extend(&outer.data);
    let at = bytes.len() as u32;
    bytes.extend(&cd);
    bytes.extend(end_record(5, cd.len() as u32, at));
    assert_eq!(reason("overlap", bytes), "zip entries overlap.");

    // The local header names another part than the directory does.
    let mut mismatch = entries(vec![raw("word/a.png", b"x")]);
    let at = mismatch
        .windows(10)
        .position(|w| w == b"word/a.png")
        .unwrap();
    mismatch[at + 5] = b'b';
    assert_eq!(
        reason("mismatch", mismatch),
        "its zip index doesn't match its contents."
    );
}

#[test]
fn sizes_are_capped_before_and_while_reading() {
    let big = Run::new(None);
    let dir = temp_dir("caps");

    let mut lies = build(&docx());
    patch_size(&mut lies, "[Content_Types].xml", 10);
    let e = refused("lies", &lies);
    assert_eq!(
        e.message,
        "'in.docx' is damaged or unsafe: part [Content_Types].xml is larger than its header says."
    );

    let mut claims = build(&docx());
    patch_size(&mut claims, "[Content_Types].xml", 100 << 20);
    let e = refused("claims", &claims);
    assert_eq!(e.code, "limit_exceeded");
    assert_eq!(
        e.message,
        "Part [Content_Types].xml of 'in.docx' is over the limit: 100 MiB (at most 64 MiB)."
    );

    // Over 512 MiB is refused from its size alone (the file is never read).
    let p = dir.join("huge.pptx");
    std::fs::File::create(&p)
        .unwrap()
        .set_len((512 << 20) + 1)
        .unwrap();
    let t = Instant::now();
    let e = Package::open(&p, 1, &big).unwrap_err();
    std::fs::remove_file(&p).unwrap();
    assert!(t.elapsed() < Duration::from_millis(200));
    assert_eq!(
        e.message,
        "'huge.pptx' is over the limit: 513 MiB (at most 512 MiB)."
    );

    let many: Vec<Raw> = (0..10_001)
        .map(|i| raw(&format!("x/{i}.png"), b""))
        .collect();
    let e = refused("entries", &raw_zip(&many));
    assert_eq!(
        e.message,
        "The entry count of 'in.docx' is over the limit: 10,001 (at most 10,000)."
    );

    // The run's allowance covers every input together.
    let p = write(&dir, "a.docx", &build(&docx()));
    let size = std::fs::read(&p).unwrap().len() as u64 * 4;
    let tight = Run::with_limit(None, size);
    let e = (1..10)
        .find_map(|i| Package::open(&p, i, &tight).err())
        .unwrap();
    assert_eq!(e.code, "limit_exceeded");
}

#[test]
fn signed_packages_are_flagged() {
    let sig =
        "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";
    let mut pkg = minimal(
        "docx",
        &[(
            "_xmlsignatures/origin.sigs",
            "application/vnd.openxmlformats-package.digital-signature-origin",
            "",
        )],
    );
    set(
        &mut pkg,
        "_rels/.rels",
        rels(&[
            ("rId1", DOC_REL, "word/document.xml"),
            ("rId2", sig, "_xmlsignatures/origin.sigs"),
        ])
        .as_bytes(),
    );
    let p = write(&temp_dir("signed"), "s.docx", &build(&pkg));
    assert!(open(&p).unwrap().signed);
}

#[test]
fn extra_bytes_around_the_zip_are_ignored() {
    let mut bytes = b"#!prepended bytes\n".to_vec();
    bytes.extend(build(&docx()));
    let p = write(&temp_dir("prepended"), "a.docx", &bytes);
    assert_eq!(open(&p).unwrap().kind, Kind::Docx);
}

#[test]
fn inputs_must_share_a_family() {
    let dir = temp_dir("mixed");
    let paths: Vec<PathBuf> = ["docx", "dotx", "pptx", "potx", "ppsx"]
        .iter()
        .map(|k| write(&dir, &format!("f.{k}"), &build(&minimal(k, &[]))))
        .collect();
    assert_eq!(open_inputs(&paths[..2], &Run::new(None)).unwrap().len(), 2);
    let e = open_inputs(&paths, &Run::new(None)).unwrap_err();
    assert_eq!(e.code, "mixed_formats");
    assert_eq!(
        e.message,
        "The files mix 2 Word documents and 3 PowerPoint decks; doc-merge merges one kind at a time."
    );
    assert_eq!(e.input, None);
}

/// Every part of every committed fixture, and of the packages embedded in them, inflated.
fn fixture_parts(name: &str, bytes: &[u8], out: &mut Vec<(String, Vec<u8>)>) {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let mut b = vec![];
        std::io::Read::read_to_end(&mut f, &mut b).unwrap();
        let part = format!("{name}/{}", f.name().unwrap());
        if b.starts_with(b"PK\x03\x04") {
            fixture_parts(&part, &b, out);
        }
        out.push((part, b));
    }
}

#[test]
fn fixtures_carry_no_personal_data() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut parts = vec![];
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x != "ps1") {
            let bytes = std::fs::read(&p).unwrap();
            assert!(bytes.len() <= 500 << 10, "{} is over 500 KB", p.display());
            fixture_parts(
                &p.file_name().unwrap().to_string_lossy(),
                &bytes,
                &mut parts,
            );
        }
    }
    assert!(parts.len() > 50, "only {} parts", parts.len());
    let email = |t: &str| {
        t.split(|c: char| c.is_whitespace() || "<>\"'=:;,()[]".contains(c))
            .any(|w| {
                w.split_once('@')
                    .is_some_and(|(u, d)| !u.is_empty() && d.contains('.'))
            })
    };
    for (name, bytes) in parts {
        let t = String::from_utf8_lossy(&bytes);
        for bad in [r":\", r"\Users", "/Users/", "/home/"] {
            assert!(!t.contains(bad), "{name} holds {bad}");
        }
        assert!(!email(&t), "{name} holds an email address");
        for at in t.match_indices("providerId=\"").map(|(i, m)| i + m.len()) {
            assert!(t[at..].starts_with("None\""), "{name} names an account");
        }
    }
}
