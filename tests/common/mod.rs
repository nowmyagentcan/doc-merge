//! Package builders for tests: real packages from XML strings, hand-made ZIP bytes for the
//! hostile cases, and readers for checking what was written.
#![allow(dead_code)]

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use doc_merge::error::Error;
use doc_merge::paths::Target;
use doc_merge::{Options, Report};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

pub const CT_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
pub const RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
pub const DOC_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
pub const IMAGE_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";

/// A fresh folder for one test's files.
pub fn temp_dir(test: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("doc-merge-test-{}-{test}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Merges `paths` into exactly `output`.
pub fn merge(
    paths: &[PathBuf],
    output: &Path,
    options: &Options,
    deadline: Option<Instant>,
) -> Result<Report, Error> {
    let target = Target {
        output: Some(output.into()),
        overwrite: true,
    };
    doc_merge::merge(paths, &target, options, deadline)
}

pub fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

/// The timestamp of every entry the builders write.
pub fn source_time() -> DateTime {
    DateTime::from_date_and_time(2024, 5, 6, 7, 8, 10).unwrap()
}

/// A ZIP of `parts`, deflated, in order.
pub fn zip(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    let o = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(source_time());
    for (name, data) in parts {
        z.start_file(*name, o).unwrap();
        z.write_all(data).unwrap();
    }
    z.finish().unwrap().into_inner()
}

/// `[Content_Types].xml` with the usual defaults plus `overrides` (part name, type).
pub fn content_types(overrides: &[(&str, &str)]) -> String {
    let mut s = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="{CT_NS}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/>"#
    );
    for (part, t) in overrides {
        s += &format!(r#"<Override PartName="/{part}" ContentType="{t}"/>"#);
    }
    s + "</Types>"
}

/// A relationships part: (id, type, target), with `External` targets marked by a leading `!`.
pub fn rels(list: &[(&str, &str, &str)]) -> String {
    let mut s = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{RELS_NS}">"#
    );
    for (id, t, target) in list {
        match target.strip_prefix('!') {
            Some(url) => {
                s += &format!(
                    r#"<Relationship Id="{id}" Type="{t}" Target="{url}" TargetMode="External"/>"#
                )
            }
            None => s += &format!(r#"<Relationship Id="{id}" Type="{t}" Target="{target}"/>"#),
        }
    }
    s + "</Relationships>"
}

/// The main part, its type and its root element for each kind.
pub fn kind_parts(kind: &str) -> (&'static str, String, &'static str) {
    let o = "application/vnd.openxmlformats-officedocument.";
    let (part, t, xml) = match kind {
        "docx" => (
            "word/document.xml",
            "wordprocessingml.document.main+xml",
            "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body/></w:document>",
        ),
        "dotx" => (
            "word/document.xml",
            "wordprocessingml.template.main+xml",
            "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body/></w:document>",
        ),
        "pptx" => (
            "ppt/presentation.xml",
            "presentationml.presentation.main+xml",
            "<p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"/>",
        ),
        "potx" => (
            "ppt/presentation.xml",
            "presentationml.template.main+xml",
            "<p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"/>",
        ),
        "ppsx" => (
            "ppt/presentation.xml",
            "presentationml.slideshow.main+xml",
            "<p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"/>",
        ),
        "xlsx" => (
            "xl/workbook.xml",
            "spreadsheetml.sheet.main+xml",
            "<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"/>",
        ),
        "xltx" => (
            "xl/workbook.xml",
            "spreadsheetml.template.main+xml",
            "<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"/>",
        ),
        _ => panic!("{kind}"),
    };
    (part, format!("{o}{t}"), xml)
}

/// The parts of a minimal package of `kind`, with `extra` parts (name, type, xml) and
/// extra package relationships after the main one.
pub fn minimal(kind: &str, extra: &[(&str, &str, &str)]) -> Vec<(String, Vec<u8>)> {
    let (main, t, xml) = kind_parts(kind);
    let mut overrides = vec![(main, t.as_str())];
    overrides.extend(extra.iter().filter(|e| !e.1.is_empty()).map(|e| (e.0, e.1)));
    let mut parts = vec![
        (
            "[Content_Types].xml".to_string(),
            content_types(&overrides).into_bytes(),
        ),
        (
            "_rels/.rels".to_string(),
            rels(&[("rId1", DOC_REL, main)]).into_bytes(),
        ),
        (main.to_string(), xml.as_bytes().to_vec()),
    ];
    parts.extend(
        extra
            .iter()
            .map(|e| (e.0.to_string(), e.2.as_bytes().to_vec())),
    );
    parts
}

pub fn build(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
    let list: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    zip(&list)
}

/// Word files no merge gets past, each with the name it goes by and the code that refuses it.
pub fn hostile() -> Vec<(&'static str, Vec<u8>, &'static str)> {
    let word = |main: &str| {
        let mut parts = minimal("docx", &[]);
        set(&mut parts, "word/document.xml", main.as_bytes());
        build(&parts)
    };
    let body = |inner: &str| {
        let ns = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
        format!("<w:document xmlns:w=\"{ns}\"><w:body>{inner}</w:body></w:document>")
    };
    let deep = "<w:p>".repeat(100_000) + &"</w:p>".repeat(100_000);
    let laughs = "<!DOCTYPE d [<!ENTITY a \"aaaaaaaaaa\"><!ENTITY b \"&a;&a;&a;&a;\">]>";
    let mut claims = build(&minimal("docx", &[]));
    patch_size(&mut claims, "word/document.xml", 1 << 30);
    let mut lies = build(&minimal("docx", &[]));
    patch_size(&mut lies, "word/document.xml", 10);
    let mut macros = minimal(
        "docx",
        &[(
            "word/vbaProject.bin",
            "application/vnd.ms-office.vbaProject",
            "x",
        )],
    );
    set(&mut macros, "word/document.xml", body("").as_bytes());
    vec![
        ("garbage.docx", vec![0x55; 4096], "not_office_file"),
        (
            "old.doc",
            b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1 and more".to_vec(),
            "legacy_or_encrypted",
        ),
        ("macros.docx", build(&macros), "macro_enabled"),
        ("claims.docx", claims, "limit_exceeded"),
        ("lies.docx", lies, "bad_package"),
        ("deep.docx", word(&body(&deep)), "bad_package"),
        (
            "laughs.docx",
            word(&(laughs.to_string() + &body("<w:p/>"))),
            "bad_package",
        ),
        ("broken.docx", word("<w:document><w:body>"), "bad_package"),
    ]
}

/// Replaces part `name` (or adds it at the end).
pub fn set(parts: &mut Vec<(String, Vec<u8>)>, name: &str, data: &[u8]) {
    match parts.iter_mut().find(|p| p.0 == name) {
        Some(p) => p.1 = data.to_vec(),
        None => parts.push((name.into(), data.to_vec())),
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Four bytes that, appended to `data`, give a CRC-32 of `target`.
pub fn forge_crc(data: &[u8], target: u32) -> [u8; 4] {
    let mut want = !target;
    for _ in 0..32 {
        want = if want & 0x8000_0000 != 0 {
            ((want ^ 0xEDB8_8320) << 1) | 1
        } else {
            want << 1
        };
    }
    (want ^ !crc32(data)).to_le_bytes()
}

/// One hand-made ZIP entry, stored unless `method` says otherwise.
#[derive(Clone)]
pub struct Raw {
    pub name: String,
    pub data: Vec<u8>,
    pub method: u16,
    pub flags: u16,
    /// The uncompressed size written in the headers (default: the data's length).
    pub size: Option<u32>,
}

pub fn raw(name: &str, data: &[u8]) -> Raw {
    Raw {
        name: name.into(),
        data: data.to_vec(),
        method: 0,
        flags: 0,
        size: None,
    }
}

pub fn local_header(e: &Raw) -> Vec<u8> {
    let mut b = b"PK\x03\x04".to_vec();
    b.extend(20u16.to_le_bytes());
    b.extend(e.flags.to_le_bytes());
    b.extend(e.method.to_le_bytes());
    b.extend([0, 0, 0x21, 0]);
    b.extend(crc32(&e.data).to_le_bytes());
    b.extend((e.data.len() as u32).to_le_bytes());
    b.extend(e.size.unwrap_or(e.data.len() as u32).to_le_bytes());
    b.extend((e.name.len() as u16).to_le_bytes());
    b.extend(0u16.to_le_bytes());
    b.extend(e.name.as_bytes());
    b
}

pub fn central_header(e: &Raw, offset: u32) -> Vec<u8> {
    let mut c = b"PK\x01\x02".to_vec();
    c.extend(20u16.to_le_bytes());
    // Version needed through the extra length are the local header's; then comment
    // length, disk, internal and external attributes.
    c.extend(&local_header(e)[4..30]);
    c.extend([0u8; 10]);
    c.extend(offset.to_le_bytes());
    c.extend(e.name.as_bytes());
    c
}

pub fn end_record(count: u16, cd_size: u32, cd_offset: u32) -> Vec<u8> {
    let mut b = b"PK\x05\x06".to_vec();
    b.extend([0u8; 4]);
    b.extend(count.to_le_bytes());
    b.extend(count.to_le_bytes());
    b.extend(cd_size.to_le_bytes());
    b.extend(cd_offset.to_le_bytes());
    b.extend(0u16.to_le_bytes());
    b
}

/// A ZIP of hand-made entries: local headers and data, the central directory, the end.
pub fn raw_zip(entries: &[Raw]) -> Vec<u8> {
    let mut out = vec![];
    let mut cd = vec![];
    for e in entries {
        cd.extend(central_header(e, out.len() as u32));
        out.extend(local_header(e));
        out.extend(&e.data);
    }
    let at = out.len() as u32;
    out.extend(&cd);
    out.extend(end_record(entries.len() as u16, cd.len() as u32, at));
    out
}

/// Sets the uncompressed size of entry `name` in both its headers, leaving the data alone.
pub fn patch_size(zip: &mut [u8], name: &str, size: u32) {
    let n = name.as_bytes();
    for at in 0..zip.len().saturating_sub(46 + n.len()) {
        let local = zip[at..].starts_with(b"PK\x03\x04") && zip[at + 30..].starts_with(n);
        let central = zip[at..].starts_with(b"PK\x01\x02") && zip[at + 46..].starts_with(n);
        let off = if local {
            22
        } else if central {
            24
        } else {
            continue;
        };
        zip[at + off..at + off + 4].copy_from_slice(&size.to_le_bytes());
    }
}

/// The entries of a written ZIP: (name, bytes) in order.
pub fn entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut z = ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut b = vec![];
            f.read_to_end(&mut b).unwrap();
            (f.name().unwrap().into_owned(), b)
        })
        .collect()
}

/// The entries of a written ZIP as stored: (name, compressed bytes, timestamp).
pub fn stored(path: &Path) -> Vec<(String, Vec<u8>, DateTime)> {
    let mut z = ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index_raw(i).unwrap();
            let mut b = vec![];
            f.read_to_end(&mut b).unwrap();
            let time = f.last_modified().unwrap();
            (f.name().unwrap().into_owned(), b, time)
        })
        .collect()
}

pub fn text(entries: &[(String, Vec<u8>)], name: &str) -> String {
    let e = entries
        .iter()
        .find(|e| e.0 == name)
        .unwrap_or_else(|| panic!("no part {name}"));
    String::from_utf8(e.1.clone()).unwrap()
}
