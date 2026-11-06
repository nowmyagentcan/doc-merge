//! Opening one input: the file within its size limit, the ZIP's central directory with every
//! entry checked, the content types and package relationships, and the kind of document,
//! decided from the package and never from the file name. Parts are inflated only when asked
//! for, each within its cap and the run's allowance.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use quick_xml::events::Event;
use schemars::JsonSchema;
use serde::Serialize;
use zip::result::ZipError;
use zip::{CompressionMethod, ZipArchive};

use crate::error::{Error, Foreign, mib, thousands};
use crate::xml::{self, Reader};
use crate::{Origin, Run};

pub const MAX_FILE: u64 = 512 << 20;
pub const MAX_ENTRIES: u64 = 10_000;
/// Parts parsed whole into memory: registries, relationships, content types.
pub const MAX_PARSED: u64 = 64 << 20;
/// Parts read as a stream or held once: document bodies and worksheets.
pub const MAX_STREAMED: u64 = 512 << 20;

pub const CONTENT_TYPES: &str = "[Content_Types].xml";
pub const ROOT_RELS: &str = "_rels/.rels";
pub const RELS_TYPE: &str = "application/vnd.openxmlformats-package.relationships+xml";
pub const OFFICE_DOCUMENT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
const STRICT_DOCUMENT: &str =
    "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument";
pub const SIGNATURE_ORIGIN: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";
const OOXML: &str = "application/vnd.openxmlformats-officedocument.";
const XLSB: &str = "application/vnd.ms-excel.sheet.binary.macroEnabled.main";
const MACRO_PARTS: [&str; 3] = [
    "application/vnd.ms-office.vbaProject",
    "application/vnd.ms-excel.macrosheet+xml",
    "application/vnd.ms-excel.intlmacrosheet+xml",
];
const CFB: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Docx,
    Dotx,
    Pptx,
    Potx,
    Ppsx,
    Xlsx,
    Xltx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Word,
    PowerPoint,
    Excel,
}

impl Kind {
    const ALL: [(Kind, &'static str); 7] = [
        (Kind::Docx, "wordprocessingml.document.main+xml"),
        (Kind::Dotx, "wordprocessingml.template.main+xml"),
        (Kind::Pptx, "presentationml.presentation.main+xml"),
        (Kind::Potx, "presentationml.template.main+xml"),
        (Kind::Ppsx, "presentationml.slideshow.main+xml"),
        (Kind::Xlsx, "spreadsheetml.sheet.main+xml"),
        (Kind::Xltx, "spreadsheetml.template.main+xml"),
    ];

    fn from_content_type(t: &str) -> Option<Kind> {
        let rest = t.strip_prefix(OOXML)?;
        Self::ALL.iter().find(|(_, s)| *s == rest).map(|(k, _)| *k)
    }

    pub fn content_type(self) -> String {
        let suffix = Self::ALL.iter().find(|(k, _)| *k == self).unwrap().1;
        format!("{OOXML}{suffix}")
    }

    pub fn family(self) -> Family {
        match self {
            Kind::Docx | Kind::Dotx => Family::Word,
            Kind::Pptx | Kind::Potx | Kind::Ppsx => Family::PowerPoint,
            Kind::Xlsx | Kind::Xltx => Family::Excel,
        }
    }
}

impl Family {
    /// The extension of the family's merged files.
    pub fn extension(self) -> &'static str {
        match self {
            Family::Word => "docx",
            Family::PowerPoint => "pptx",
            Family::Excel => "xlsx",
        }
    }

    /// The kind every merged output of this family is.
    pub fn output(self) -> Kind {
        match self {
            Family::Word => Kind::Docx,
            Family::PowerPoint => Kind::Pptx,
            Family::Excel => Kind::Xlsx,
        }
    }
}

/// One relationship of a part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rel {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub external: bool,
}

/// `[Content_Types].xml`: types by extension and by part name, both compared ignoring ASCII
/// case. Part names are kept without their leading `/`.
#[derive(Debug, Clone, Default)]
pub struct ContentTypes {
    pub defaults: Vec<(String, String)>,
    pub overrides: Vec<(String, String)>,
    by_part: HashMap<String, usize>,
}

impl ContentTypes {
    pub fn parse(text: &str, origin: &Origin) -> Result<Self, Error> {
        let mut types = Self::default();
        let mut r = Reader::new(text, CONTENT_TYPES, origin);
        loop {
            match r.event()? {
                Event::Eof => return Ok(types),
                Event::Start(e) | Event::Empty(e) if r.depth() <= 1 => {
                    let attr = |name: &str| -> Result<String, Error> {
                        let found = r
                            .attributes(&e)?
                            .into_iter()
                            .find(|a| a.0.is_empty() && a.1 == name);
                        Ok(found.map(|a| a.2).unwrap_or_default())
                    };
                    match e.local_name().as_ref() {
                        "Default" => types
                            .defaults
                            .push((attr("Extension")?, attr("ContentType")?)),
                        "Override" => {
                            let part = attr("PartName")?;
                            types.add_override(part.trim_start_matches('/'), &attr("ContentType")?);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    pub fn add_override(&mut self, part: &str, content_type: &str) {
        self.by_part
            .insert(part.to_ascii_lowercase(), self.overrides.len());
        self.overrides.push((part.into(), content_type.into()));
    }

    /// Drops the overrides of the parts `keep` refuses.
    pub fn retain_overrides(&mut self, keep: impl Fn(&str) -> bool) {
        self.overrides.retain(|(p, _)| keep(p));
        self.by_part = self
            .overrides
            .iter()
            .enumerate()
            .map(|(i, (p, _))| (p.to_ascii_lowercase(), i))
            .collect();
    }

    pub fn override_of(&self, part: &str) -> Option<&str> {
        let i = *self.by_part.get(&part.to_ascii_lowercase())?;
        Some(&self.overrides[i].1)
    }

    pub fn default_of(&self, extension: &str) -> Option<&str> {
        let d = self
            .defaults
            .iter()
            .find(|(e, _)| e.eq_ignore_ascii_case(extension));
        d.map(|(_, t)| t.as_str())
    }

    /// A part's content type: its override, else the default for its extension.
    pub fn of(&self, part: &str) -> Option<&str> {
        self.override_of(part)
            .or_else(|| self.default_of(extension(part)?))
    }
}

/// The extension of a part name, if its last segment has one.
pub fn extension(part: &str) -> Option<&str> {
    let last = part.rsplit('/').next()?;
    last.rsplit_once('.').map(|(_, e)| e)
}

/// The relationships part of `part` (`""` for the package itself).
pub fn rels_name(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// The part an internal relationship target of `source` points at, or none when it leaves
/// the package. `source` is `""` for the package's own relationships.
pub fn resolve(source: &str, target: &str) -> Option<String> {
    let target = target.split('#').next()?;
    let mut segments: Vec<&str> = match target.strip_prefix('/') {
        Some(_) => vec![],
        None => source.split('/').collect(),
    };
    segments.pop();
    for s in target.split('/') {
        match s {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            s => segments.push(s),
        }
    }
    (!segments.is_empty()).then(|| segments.join("/"))
}

/// `%XX` escapes decoded, when they make valid UTF-8.
pub fn unescape(s: &str) -> Option<String> {
    if !s.contains('%') {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (b[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

pub fn parse_rels(text: &str, part: &str, origin: &Origin) -> Result<Vec<Rel>, Error> {
    let mut rels = vec![];
    let mut r = Reader::new(text, part, origin);
    loop {
        match r.event()? {
            Event::Eof => return Ok(rels),
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "Relationship" => {
                let mut rel = Rel {
                    id: String::new(),
                    kind: String::new(),
                    target: String::new(),
                    external: false,
                };
                for (ns, name, value) in r.attributes(&e)? {
                    match (ns, name.as_str()) {
                        ("", "Id") => rel.id = value,
                        ("", "Type") => rel.kind = value,
                        ("", "Target") => rel.target = value,
                        ("", "TargetMode") => rel.external = value == "External",
                        _ => {}
                    }
                }
                rels.push(rel);
            }
            _ => {}
        }
    }
}

/// Where an input's bytes come from: a file, or an upload held in memory.
pub trait Source: Read + Seek {}
impl<T: Read + Seek> Source for T {}

/// A file's bytes, shared between the page's list and a merge under way.
pub type Upload = Arc<Vec<u8>>;

struct Held(Upload);

impl AsRef<[u8]> for Held {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A ZIP entry that is a part (folder entries are left out).
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// Its position in the ZIP, for raw copies.
    pub index: usize,
    pub size: u64,
    pub compressed: u64,
    pub crc: u32,
    pub time: Option<zip::DateTime>,
}

/// One opened input.
pub struct Package {
    pub origin: Origin,
    pub path: PathBuf,
    pub kind: Kind,
    /// The main part, e.g. `word/document.xml`.
    pub main: String,
    /// Whether it carries a digital signature, which the merged file can't keep.
    pub signed: bool,
    pub types: ContentTypes,
    /// The package's own relationships.
    pub rels: Vec<Rel>,
    /// In ZIP order.
    pub entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
    zip: ZipArchive<Box<dyn Source>>,
}

impl std::fmt::Debug for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Package")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .finish()
    }
}

/// The name a file is called by in messages: its file name as given.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

impl Package {
    /// Opens the `input`-th (1-based) file and runs every check that needs no part beyond the
    /// content types and the package relationships.
    pub fn open(path: &Path, input: usize, run: &Rc<Run>) -> Result<Package, Error> {
        let name = display_name(path);
        let at = |e: Error| e.at(input);
        let meta = std::fs::metadata(path)
            .map_err(|_| at(Error::file_not_found(&path.to_string_lossy())))?;
        if !meta.is_file() {
            return Err(at(Error::not_office_file(&name)));
        }
        if meta.len() > MAX_FILE {
            return Err(at(Error::limit_exceeded(
                &format!("'{name}'"),
                &mib(meta.len()),
                "512 MiB",
            )));
        }
        let file =
            File::open(path).map_err(|_| at(Error::file_not_found(&path.to_string_lossy())))?;
        let source = Box::new(BufReader::new(file));
        Self::read_zip(source, meta.len(), path, &name, input, run)
    }

    /// Opens an upload called `name` as the `input`-th (1-based) file, with the same checks.
    pub fn from_upload(
        name: &str,
        bytes: Upload,
        input: usize,
        run: &Rc<Run>,
    ) -> Result<Package, Error> {
        let len = bytes.len() as u64;
        let source = Box::new(Cursor::new(Held(bytes)));
        Self::read_zip(source, len, Path::new(name), name, input, run)
    }

    fn read_zip(
        mut file: Box<dyn Source>,
        len: u64,
        path: &Path,
        name: &str,
        input: usize,
        run: &Rc<Run>,
    ) -> Result<Package, Error> {
        let origin = Origin {
            name: name.into(),
            input,
            run: run.clone(),
        };
        let at = |e: Error| e.at(input);
        let mut head = [0u8; 8];
        let n = file.read(&mut head).unwrap_or(0);
        if head[..n] == CFB {
            return Err(at(Error::legacy_or_encrypted(name)));
        }
        match declared_entries(&mut file, len) {
            None => return Err(at(Error::not_office_file(name))),
            Some(n) if n > MAX_ENTRIES => {
                return Err(at(Error::limit_exceeded(
                    &format!("The entry count of '{name}'"),
                    &thousands(n),
                    "10,000",
                )));
            }
            Some(_) => {}
        }
        let unreadable =
            |e: &dyn std::error::Error| origin.bad("its zip structure can't be read").because(e);
        file.seek(SeekFrom::Start(0)).map_err(|e| unreadable(&e))?;
        let mut zip = ZipArchive::new(file).map_err(|e| unreadable(&e))?;

        let mut entries = vec![];
        let mut by_name = HashMap::new();
        let mut times = BTreeMap::new();
        for index in 0..zip.len() {
            let e = zip.by_index_data(index).map_err(|e| unreadable(&e))?;
            if let Some(t) = e.last_modified().filter(|_| e.header_start() < len) {
                let bytes = [t.timepart().to_le_bytes(), t.datepart().to_le_bytes()].concat();
                times.extend((e.header_start() + 10..).zip(bytes));
            }
            let part = e.name().map_err(|e| unreadable(&e))?.into_owned();
            if part.ends_with('/') && e.size() == 0 {
                continue;
            }
            if !allowed_name(&part) {
                return Err(origin.bad(&format!("part name {part} is not allowed")));
            }
            if by_name
                .insert(part.to_ascii_lowercase(), entries.len())
                .is_some()
            {
                return Err(origin.bad(&format!("two parts are named {part}")));
            }
            if e.encrypted() {
                return Err(origin.bad(&format!("part {part} is encrypted")));
            }
            let method = e.compression();
            if !matches!(
                method,
                CompressionMethod::Stored | CompressionMethod::Deflated
            ) {
                let n = u16::from(method.into_compression());
                return Err(origin.bad(&format!("part {part} uses compression method {n}")));
            }
            entries.push(Entry {
                name: part,
                index,
                size: e.size(),
                compressed: e.compressed_size(),
                crc: e.crc32(),
                time: e.last_modified(),
            });
        }
        let damaged = |e: ZipError| {
            let overlap = e.to_string().contains("overlapping");
            origin.bad(if overlap {
                "zip entries overlap"
            } else {
                "its zip index doesn't match its contents"
            })
        };
        match zip.validate_header_consistency() {
            Err(e) if e.to_string().contains("last modified time") => {
                zip = with_central_times(zip, times).map_err(damaged)?;
            }
            other => other.map_err(damaged)?,
        }

        let mut pkg = Package {
            origin: origin.clone(),
            path: path.to_path_buf(),
            kind: Kind::Docx,
            main: String::new(),
            signed: false,
            types: ContentTypes::default(),
            rels: vec![],
            entries,
            by_name,
            zip,
        };
        if pkg.has("mimetype")
            && pkg
                .read("mimetype", 256)?
                .starts_with(b"application/vnd.oasis.opendocument")
        {
            return Err(at(Error::unsupported_format(name, Foreign::OpenDocument)));
        }
        match (pkg.has(CONTENT_TYPES), pkg.has(ROOT_RELS)) {
            (false, false) => return Err(at(Error::not_office_file(name))),
            (false, true) => return Err(origin.bad("[Content_Types].xml is missing")),
            (true, false) => return Err(origin.bad("the main part is missing")),
            (true, true) => {}
        }
        pkg.types = ContentTypes::parse(&pkg.text(CONTENT_TYPES, MAX_PARSED)?, &origin)?;
        pkg.rels = parse_rels(&pkg.text(ROOT_RELS, MAX_PARSED)?, ROOT_RELS, &origin)?;

        if pkg.rels.iter().any(|r| r.kind == STRICT_DOCUMENT) {
            return Err(at(Error::strict_ooxml(name)));
        }
        let main = pkg
            .rels
            .iter()
            .find(|r| r.kind == OFFICE_DOCUMENT && !r.external);
        let Some(main) = main.and_then(|r| resolve("", &r.target)) else {
            return Err(at(Error::unsupported_format(name, Foreign::OtherZip)));
        };
        let Some(main) = pkg.find(&main).map(|e| e.name.clone()) else {
            return Err(origin.bad("the main part is missing"));
        };
        let main_type = pkg.types.of(&main).unwrap_or_default();
        if main_type == XLSB {
            return Err(at(Error::unsupported_format(name, Foreign::Xlsb)));
        }
        if main_type.contains("macroEnabled") {
            return Err(at(Error::macro_enabled(name)));
        }
        let Some(kind) = Kind::from_content_type(main_type) else {
            return Err(at(Error::unsupported_format(name, Foreign::OtherZip)));
        };
        for e in &pkg.entries {
            if e.name == CONTENT_TYPES {
                continue;
            }
            match pkg.types.of(&e.name) {
                None => return Err(origin.bad(&format!("part {} has no content type", e.name))),
                Some(t) if MACRO_PARTS.contains(&t) => return Err(at(Error::macro_enabled(name))),
                Some(_) => {}
            }
        }
        pkg.signed = pkg.entries.iter().any(|e| is_signature(&e.name))
            || pkg.rels.iter().any(|r| r.kind == SIGNATURE_ORIGIN);
        pkg.kind = kind;
        pkg.main = main;
        Ok(pkg)
    }

    /// The entry of a part, by its name compared ignoring ASCII case (and, failing that,
    /// with `%XX` escapes decoded).
    pub fn find(&self, part: &str) -> Option<&Entry> {
        self.position(part).map(|i| &self.entries[i])
    }

    /// Where [`Package::find`]'s entry is in `entries`.
    pub fn position(&self, part: &str) -> Option<usize> {
        let get = |p: &str| self.by_name.get(&p.to_ascii_lowercase()).copied();
        get(part).or_else(|| get(&unescape(part)?))
    }

    pub fn has(&self, part: &str) -> bool {
        self.find(part).is_some()
    }

    /// A part's bytes, refused when it would inflate to more than `cap`.
    pub fn read(&mut self, part: &str, cap: u64) -> Result<Vec<u8>, Error> {
        self.origin.run.on_time()?;
        let Some(e) = self.find(part).cloned() else {
            return Err(self.origin.bad(&format!("part {part} is missing")));
        };
        if e.size > cap {
            return Err(too_big(&e.name, &self.origin, e.size, &mib(cap)));
        }
        self.origin.run.inflate(e.size)?;
        let larger = || {
            self.origin
                .bad(&format!("part {} is larger than its header says", e.name))
        };
        // Deflate can't grow data more than 1,032 times, so a size that lies reserves nothing.
        let expected = e
            .size
            .min(e.compressed.saturating_mul(1032).saturating_add(64));
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(expected as usize)
            .map_err(|_| too_big(&e.name, &self.origin, e.size, "the memory free"))?;
        let read = self
            .zip
            .by_index(e.index)
            .and_then(|f| Ok(f.take(e.size + 1).read_to_end(&mut bytes)?));
        match read {
            Err(err) if err.to_string().contains("larger than its declared") => Err(larger()),
            Err(err) => Err(self
                .origin
                .bad(&format!("part {} can't be read ({err})", e.name))),
            Ok(n) if n as u64 > e.size => Err(larger()),
            Ok(_) => Ok(bytes),
        }
    }

    /// A part as text (see [`xml::decode`]).
    pub fn text(&mut self, part: &str, cap: u64) -> Result<String, Error> {
        let bytes = self.read(part, cap)?;
        Ok(xml::decode(&bytes, part, &self.origin)?.into_owned())
    }

    /// The relationships of a part; none when it has no relationships part.
    pub fn rels_of(&mut self, part: &str) -> Result<Vec<Rel>, Error> {
        let name = rels_name(part);
        match self.find(&name).map(|e| e.name.clone()) {
            Some(name) => parse_rels(&self.text(&name, MAX_PARSED)?, &name, &self.origin.clone()),
            None => Ok(vec![]),
        }
    }

    /// The entry as stored, for copying it without inflating.
    pub fn raw(&mut self, entry: &Entry) -> Result<zip::read::ZipFile<'_, Box<dyn Source>>, Error> {
        let bad = |_| {
            self.origin
                .bad(&format!("part {} can't be read", entry.name))
        };
        self.zip.by_index_raw(entry.index).map_err(bad)
    }
}

/// `zip` once its headers agree in all but the time: Office's own templates differ there,
/// which changes nothing in the package.
fn with_central_times(
    zip: ZipArchive<Box<dyn Source>>,
    times: BTreeMap<u64, u8>,
) -> Result<ZipArchive<Box<dyn Source>>, ZipError> {
    let source = zip.into_inner();
    let mut view = ZipArchive::new(CentralTimes {
        source,
        times,
        at: 0,
    })?;
    view.validate_header_consistency()?;
    ZipArchive::new(view.into_inner().source)
}

/// A source read as if each local zip header had the central directory's time.
struct CentralTimes {
    source: Box<dyn Source>,
    times: BTreeMap<u64, u8>,
    at: u64,
}

impl Read for CentralTimes {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.source.read(buf)?;
        for (at, byte) in self.times.range(self.at..self.at + n as u64) {
            buf[(at - self.at) as usize] = *byte;
        }
        self.at += n as u64;
        Ok(n)
    }
}

impl Seek for CentralTimes {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.at = self.source.seek(to)?;
        Ok(self.at)
    }
}

fn too_big(part: &str, origin: &Origin, size: u64, limit: &str) -> Error {
    let what = format!("Part {part} of '{}'", origin.name);
    Error::limit_exceeded(&what, &mib(size), limit).at(origin.input)
}

/// Whether a part belongs to a digital signature.
pub fn is_signature(part: &str) -> bool {
    part.as_bytes()
        .get(..15)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"_xmlsignatures/"))
}

/// OPC part-name rules: no empty name, leading `/`, `\`, empty, `.` or `..` segments, drive
/// letter, or trailing `.`.
fn allowed_name(part: &str) -> bool {
    let drive = part.as_bytes().get(1) == Some(&b':');
    !part.is_empty()
        && !drive
        && !part.contains('\\')
        && !part.ends_with('.')
        && part
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
}

/// The entry count the end-of-central-directory record or the ZIP64 record after it declares,
/// read before the directory itself so a hostile count is refused without parsing it; none
/// when there is no record to read it from.
fn declared_entries(file: &mut impl Source, len: u64) -> Option<u64> {
    const LOCATOR: usize = 20;
    const ZIP64_END: usize = 56;
    let tail = len.min(22 + 0xFFFF + (LOCATOR + ZIP64_END) as u64);
    file.seek(SeekFrom::Start(len - tail)).ok()?;
    let mut b = vec![0; tail as usize];
    file.read_exact(&mut b).ok()?;
    let p = b
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .filter(|p| p + 22 <= b.len())?;
    let locator = p
        .checked_sub(LOCATOR)
        .filter(|l| b[*l..].starts_with(b"PK\x06\x07"));
    let Some(locator) = locator else {
        return Some(u64::from(u16::from_le_bytes([b[p + 10], b[p + 11]])));
    };
    // Where the locator says, or just before it when bytes were added in front of the ZIP.
    let mut record = [0; ZIP64_END];
    let at = u64::from_le_bytes(b[locator + 8..locator + 16].try_into().ok()?);
    let read = file.seek(SeekFrom::Start(at)).is_ok() && file.read_exact(&mut record).is_ok();
    if !(read && record.starts_with(b"PK\x06\x06")) {
        record = b
            .get(locator.checked_sub(ZIP64_END)?..locator)?
            .try_into()
            .ok()?;
    }
    record
        .starts_with(b"PK\x06\x06")
        .then(|| u64::from_le_bytes(record[32..40].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_resolve_against_their_source() {
        assert_eq!(
            resolve("", "word/document.xml").as_deref(),
            Some("word/document.xml")
        );
        assert_eq!(
            resolve("", "/word/document.xml").as_deref(),
            Some("word/document.xml")
        );
        let slide = "ppt/slides/slide1.xml";
        assert_eq!(
            resolve(slide, "../media/image1.png").as_deref(),
            Some("ppt/media/image1.png")
        );
        assert_eq!(
            resolve(slide, "slide2.xml#x").as_deref(),
            Some("ppt/slides/slide2.xml")
        );
        assert_eq!(resolve(slide, "/ppt/a.xml").as_deref(), Some("ppt/a.xml"));
        assert_eq!(resolve(slide, "../../../x.xml"), None);
        assert_eq!(
            rels_name("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
        assert_eq!(rels_name(""), "_rels/.rels");
        assert_eq!(
            unescape("media/a%20b%C3%A9.png").as_deref(),
            Some("media/a bé.png")
        );
        assert_eq!(unescape("a%zz"), Some("a%zz".into()));
    }

    #[test]
    fn part_names_follow_opc_rules() {
        for ok in [
            "word/document.xml",
            "[Content_Types].xml",
            "_rels/.rels",
            "a",
        ] {
            assert!(allowed_name(ok), "{ok}");
        }
        for bad in [
            "",
            "/word/x.xml",
            "word\\x.xml",
            "word/../x.xml",
            "./x",
            "a//b",
            "C:x",
            "x.",
            "w/x.",
        ] {
            assert!(!allowed_name(bad), "{bad}");
        }
        assert!(is_signature("_XmlSignatures/sig1.xml"));
    }
}
