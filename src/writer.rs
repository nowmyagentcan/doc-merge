//! The merged package: a plan of parts in write order, each copied as stored from an input or
//! written new, plus the content types; then the generic consistency checks and the write
//! into a temp file beside the target, renamed into place.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use quick_xml::escape::escape;
use quick_xml::events::Event;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, System, ZipWriter};

use crate::error::{Error, Warning, Warnings, mib};
use crate::package::{
    self, CONTENT_TYPES, ContentTypes, MAX_PARSED, Package, RELS_TYPE, ROOT_RELS, Rel,
    SIGNATURE_ORIGIN, is_signature, parse_rels, rels_name, resolve,
};
use crate::xml::Reader;

/// Output entries and the output itself stay under 2 GiB (no ZIP64 is written).
pub const MAX_OUTPUT: u64 = (2 << 30) - 1;
const BUFFER: usize = 64 << 10;
pub const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// Writes a part's content into the ZIP; given the inputs, for parts streamed from one.
pub type Stream = Box<dyn FnOnce(&mut dyn Write, &mut [Package]) -> Result<(), Error>>;

pub enum Body {
    /// Entry `entry` of `inputs[input]`, copied without inflating.
    Raw {
        input: usize,
        entry: usize,
    },
    /// New or rewritten content.
    Data(Vec<u8>),
    Stream(Stream),
}

pub struct Part {
    pub name: String,
    pub body: Body,
    /// A rewritten part keeps its source's timestamp; a new one gets 1980-01-01.
    pub time: Option<DateTime>,
}

#[derive(Default)]
pub struct Plan {
    /// In write order; `[Content_Types].xml` is written first from `types`.
    pub parts: Vec<Part>,
    pub types: ContentTypes,
    types_time: Option<DateTime>,
    /// Lower-case names taken, and per numbered stem the number below which all are taken.
    taken: HashSet<String>,
    next: HashMap<String, u32>,
    /// Parts copied so far: (input, lower-case source name) to output name.
    copies: HashMap<(usize, String), String>,
    /// Media in the output as stored, by (size, CRC-32).
    media: HashMap<(u64, u32), Vec<Stored>>,
}

/// A part copied as stored: (output name, input, entry).
type Stored = (String, usize, usize);

/// Media parts over this size are never compared for sharing.
const MAX_SHARED: u64 = 64 << 20;

fn is_media(part: &str, content_type: &str) -> bool {
    part.to_ascii_lowercase().contains("/media/")
        || ["image/", "audio/", "video/"]
            .iter()
            .any(|t| content_type.starts_with(t))
}

/// Says where a relationship that a family handles itself should point instead of copying
/// its target: given the relationship and its target in the input, the output part.
pub type Mapped<'a> = &'a dyn Fn(&Rel, &str) -> Option<String>;

impl Plan {
    /// The base's entries in their order, as stored, except its signature, which the merged
    /// file can't keep; a template or slideshow becomes a document.
    pub fn base(pkg: &Package, warnings: &mut Warnings) -> Result<Self, Error> {
        let mut plan = Plan {
            types: pkg.types.clone(),
            types_time: pkg.find(CONTENT_TYPES).and_then(|e| e.time),
            ..Plan::default()
        };
        plan.types.retain_overrides(|p| !is_signature(p));
        let output = pkg.kind.family().output().content_type();
        match plan.types.override_of(&pkg.main) {
            Some(t) if *t != output => {
                let main = pkg.main.clone();
                plan.types
                    .retain_overrides(|p| !p.eq_ignore_ascii_case(&main));
                plan.types.add_override(&main, &output);
            }
            Some(_) => {}
            None if plan.types.of(&pkg.main) == Some(&output) => {}
            None => plan.types.add_override(&pkg.main, &output),
        }
        for (i, e) in pkg.entries.iter().enumerate() {
            if e.name == CONTENT_TYPES || is_signature(&e.name) {
                continue;
            }
            let body = if e.name == ROOT_RELS && pkg.signed {
                let rels: Vec<Rel> = pkg
                    .rels
                    .iter()
                    .filter(|r| r.kind != SIGNATURE_ORIGIN)
                    .cloned()
                    .collect();
                Body::Data(rels_xml(&rels))
            } else {
                Body::Raw { input: 0, entry: i }
            };
            if pkg.types.of(&e.name).is_some_and(|t| is_media(&e.name, t)) {
                plan.media
                    .entry((e.size, e.crc))
                    .or_default()
                    .push((e.name.clone(), 0, i));
            }
            plan.take(&e.name);
            plan.parts.push(Part {
                name: e.name.clone(),
                body,
                time: e.time,
            });
        }
        if pkg.signed {
            warnings.push(Warning::signature_removed(
                pkg.origin.input,
                &pkg.origin.name,
            ));
        }
        Ok(plan)
    }

    fn take(&mut self, name: &str) {
        self.taken.insert(name.to_ascii_lowercase());
    }

    fn is_taken(&self, name: &str) -> bool {
        self.taken.contains(&name.to_ascii_lowercase())
    }

    /// A free part name like `wanted`: the same folder, stem and extension with the lowest
    /// free number (`ppt/slides/slide21.xml`); a stem without digits keeps its name when it
    /// is free. Compared ignoring ASCII case.
    pub fn claim(&mut self, wanted: &str) -> String {
        let (dir, file) = wanted
            .rsplit_once('/')
            .map_or(("", wanted), |(d, f)| (d, f));
        let (stem, ext) = file
            .rsplit_once('.')
            .map_or((file, None), |(s, e)| (s, Some(e)));
        let base = stem.trim_end_matches(|c: char| c.is_ascii_digit());
        if base.len() == stem.len() && !self.is_taken(wanted) {
            self.take(wanted);
            return wanted.into();
        }
        let name = |n: u32| {
            let dir = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            let ext = ext.map(|e| format!(".{e}")).unwrap_or_default();
            format!("{dir}{base}{n}{ext}")
        };
        let key = name(0).to_ascii_lowercase();
        let mut n = self.next.get(&key).copied().unwrap_or(1);
        while self.is_taken(&name(n)) {
            n += 1;
        }
        self.next.insert(key, n + 1);
        let found = name(n);
        self.take(&found);
        found
    }

    /// Adds a part whose name was claimed (or is new and free).
    pub fn add(&mut self, name: String, body: Body, time: Option<DateTime>) {
        self.take(&name);
        self.parts.push(Part { name, body, time });
    }

    /// Replaces the body of a planned part, keeping its place and timestamp.
    pub fn replace(&mut self, name: &str, body: Body) {
        if let Some(p) = self
            .parts
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(name))
        {
            p.body = body;
        }
    }

    /// Copies part `part` of `inputs[input]` and every part it reaches through its
    /// relationships, each once (a part met again, also through a cycle, is linked), keeping
    /// every relationship id and pointing the targets at the copies. A relationship `mapped`
    /// answers for points where it says and isn't followed; external targets are never opened.
    /// A media part whose bytes equal a part already in the output is shared instead.
    /// Returns the output name of `part`.
    pub fn copy_graph(
        &mut self,
        inputs: &mut [Package],
        input: usize,
        part: &str,
        mapped: Mapped,
    ) -> Result<String, Error> {
        let (name, new) = self.place(inputs, input, part)?;
        let mut pending = if new {
            vec![(part.to_string(), name.clone())]
        } else {
            vec![]
        };
        while let Some((source, out)) = pending.pop() {
            let pkg = &mut inputs[input];
            let Some(rels_entry) = pkg.position(&rels_name(&source)) else {
                continue;
            };
            let rels = pkg.rels_of(&source)?;
            let mut copied = Vec::with_capacity(rels.len());
            for r in &rels {
                if r.external {
                    copied.push(r.clone());
                    continue;
                }
                let pkg = &inputs[input];
                let target = resolve(&source, &r.target).and_then(|t| pkg.find(&t));
                let Some(target) = target.map(|e| e.name.clone()) else {
                    let why = format!("{source} points to a missing part {}", r.target);
                    return Err(pkg.origin.bad(&why));
                };
                let to = match mapped(r, &target) {
                    Some(to) => to,
                    None => {
                        let (to, new) = self.place(inputs, input, &target)?;
                        if new {
                            pending.push((target, to.clone()));
                        }
                        to
                    }
                };
                copied.push(Rel {
                    target: relative(&out, &to),
                    ..r.clone()
                });
            }
            let body = if copied == rels {
                Body::Raw {
                    input,
                    entry: rels_entry,
                }
            } else {
                Body::Data(rels_xml(&copied))
            };
            let rels_out = rels_name(&out);
            self.type_part(&rels_out, RELS_TYPE, true);
            self.add(rels_out, body, None);
        }
        Ok(name)
    }

    /// The output name for part `part` of `inputs[input]`, and whether it is a new copy (not
    /// a part copied before, nor an equal media part already in the output).
    fn place(
        &mut self,
        inputs: &mut [Package],
        input: usize,
        part: &str,
    ) -> Result<(String, bool), Error> {
        let key = (input, part.to_ascii_lowercase());
        if let Some(name) = self.copies.get(&key) {
            return Ok((name.clone(), false));
        }
        let pkg = &inputs[input];
        let i = pkg.position(part).expect("a part of the input");
        let e = pkg.entries[i].clone();
        let source_type = pkg.types.of(&e.name).unwrap_or_default().to_string();
        let by_default = pkg.types.override_of(&e.name).is_none();
        let media = is_media(&e.name, &source_type);
        if media && e.size <= MAX_SHARED {
            let mut ours = None;
            for (name, j, k) in self
                .media
                .get(&(e.size, e.crc))
                .cloned()
                .unwrap_or_default()
            {
                if self.types.of(&name) != Some(source_type.as_str()) {
                    continue;
                }
                if ours.is_none() {
                    ours = Some(inputs[input].read(&e.name, MAX_SHARED)?);
                }
                let theirs = inputs[j].entries[k].name.clone();
                if inputs[j].read(&theirs, MAX_SHARED)? == *ours.as_ref().unwrap() {
                    self.copies.insert(key, name.clone());
                    return Ok((name, false));
                }
            }
        }
        let name = self.claim(&e.name);
        if media {
            let at = (name.clone(), input, i);
            self.media.entry((e.size, e.crc)).or_default().push(at);
        }
        self.type_part(&name, &source_type, by_default);
        self.copies.insert(key, name.clone());
        self.add(name.clone(), Body::Raw { input, entry: i }, None);
        Ok((name, true))
    }

    /// Types a new part: nothing when the output's default for its extension already says
    /// `content_type`; a new default when the source typed it by default and the output has
    /// none for the extension; an override otherwise.
    pub fn type_part(&mut self, name: &str, content_type: &str, by_default: bool) {
        let ext = package::extension(name);
        let default = ext.and_then(|e| self.types.default_of(e));
        match (default, ext) {
            (Some(t), _) if t == content_type => {}
            (None, Some(ext)) if by_default => {
                self.types.defaults.push((ext.into(), content_type.into()));
            }
            _ => self.types.add_override(name, content_type),
        }
    }
}

/// A part's relationships as the output will have them, with new ids after the highest.
pub struct Rels {
    pub list: Vec<Rel>,
    next: u32,
}

impl Rels {
    pub fn new(list: Vec<Rel>) -> Self {
        let next = list
            .iter()
            .filter_map(|r| r.id.strip_prefix("rId")?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        Self { list, next }
    }

    /// Adds a relationship and returns its new id.
    pub fn add(&mut self, kind: &str, target: String, external: bool) -> String {
        let id = loop {
            let id = format!("rId{}", self.next);
            self.next += 1;
            if !self.list.iter().any(|r| r.id == id) {
                break id;
            }
        };
        self.list.push(Rel {
            id: id.clone(),
            kind: kind.into(),
            target,
            external,
        });
        id
    }
}

/// Writes the relationships of an output part, replacing the base's when it has them.
pub fn rels_part(plan: &mut Plan, base: &Package, part: &str, rels: &Rels) {
    let name = rels_name(part);
    if base.has(&name) {
        plan.replace(&name, Body::Data(rels_xml(&rels.list)));
    } else {
        plan.type_part(&name, RELS_TYPE, true);
        plan.add(name, Body::Data(rels_xml(&rels.list)), None);
    }
}

/// A relationships part.
pub fn rels_xml(rels: &[Rel]) -> Vec<u8> {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for r in rels {
        s += &format!(
            "<Relationship Id=\"{}\" Type=\"{}\" Target=\"{}\"{}/>",
            escape(&r.id),
            escape(&r.kind),
            escape(&r.target),
            if r.external {
                " TargetMode=\"External\""
            } else {
                ""
            }
        );
    }
    s += "</Relationships>";
    s.into_bytes()
}

fn content_types_xml(types: &ContentTypes) -> Vec<u8> {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
    );
    for (ext, t) in &types.defaults {
        s += &format!(
            "<Default Extension=\"{}\" ContentType=\"{}\"/>",
            escape(ext),
            escape(t)
        );
    }
    for (part, t) in &types.overrides {
        s += &format!(
            "<Override PartName=\"/{}\" ContentType=\"{}\"/>",
            escape(part),
            escape(t)
        );
    }
    s += "</Types>";
    s.into_bytes()
}

/// The relative reference from part `from` (`""` for the package) to part `to`, with
/// characters a URI can't hold escaped.
pub fn relative(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = from
        .split('/')
        .collect::<Vec<_>>()
        .split_last()
        .map_or(vec![], |(_, d)| d.to_vec());
    let to_parts: Vec<&str> = to.split('/').collect();
    let common = from_dir
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let common = common.min(to_parts.len() - 1);
    let mut out = "../".repeat(from_dir.len() - common);
    out += &to_parts[common..].join("/");
    let mut escaped = String::with_capacity(out.len());
    for b in out.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&b) {
            escaped.push(b as char);
        } else {
            escaped += &format!("%{b:02X}");
        }
    }
    escaped
}

/// The part a relationships part belongs to (`""` for the package's own).
fn source_of(rels: &str) -> Option<String> {
    let (dir, file) = rels.rsplit_once('/')?;
    let file = file.strip_suffix(".rels")?;
    let dir = dir.strip_suffix("_rels")?.trim_end_matches('/');
    Some(if dir.is_empty() {
        file.into()
    } else {
        format!("{dir}/{file}")
    })
}

fn is_xml(content_type: &str) -> bool {
    content_type.ends_with("+xml") || content_type.ends_with("/xml")
}

/// The generic rules every merged package meets (each kind adds its own): every internal
/// relationship target exists, every relationship-id attribute of a written XML part
/// resolves in that part's relationships, every part is typed, no override names a missing
/// part, and no two names are equal ignoring case.
pub fn check(plan: &Plan, inputs: &mut [Package]) -> Result<(), Error> {
    let internal = |d: String| Err(Error::internal(&d));
    let mut names = HashSet::new();
    for p in &plan.parts {
        if !names.insert(p.name.to_ascii_lowercase()) {
            return internal(format!("two parts are named {}", p.name));
        }
        if plan.types.of(&p.name).is_none() {
            return internal(format!("part {} has no content type", p.name));
        }
    }
    if let Some((part, _)) = plan
        .types
        .overrides
        .iter()
        .find(|(o, _)| !names.contains(&o.to_ascii_lowercase()))
    {
        return internal(format!("the content types name a missing part {part}"));
    }
    let mut rels_of: HashMap<String, Vec<Rel>> = HashMap::new();
    for p in &plan.parts {
        let Some(source) = source_of(&p.name) else {
            continue;
        };
        let rels = match &p.body {
            Body::Raw { input, entry } => {
                let pkg = &mut inputs[*input];
                let name = pkg.entries[*entry].name.clone();
                parse_rels(&pkg.text(&name, MAX_PARSED)?, &name, &pkg.origin.clone())?
            }
            Body::Data(bytes) => {
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| Error::internal(&format!("part {} isn't UTF-8", p.name)))?;
                parse_rels(text, &p.name, &inputs[0].origin)?
            }
            Body::Stream(_) => continue,
        };
        for r in rels.iter().filter(|r| !r.external) {
            let target = resolve(&source, &r.target);
            if !target.is_some_and(|t| names.contains(&t.to_ascii_lowercase())) {
                return internal(format!("{} points to a missing part {}", p.name, r.target));
            }
        }
        rels_of.insert(source.to_ascii_lowercase(), rels);
    }
    for p in &plan.parts {
        let Body::Data(bytes) = &p.body else { continue };
        if !plan.types.of(&p.name).is_some_and(is_xml) {
            continue;
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::internal(&format!("part {} isn't UTF-8", p.name)))?;
        let ids: HashSet<&str> = rels_of
            .get(&p.name.to_ascii_lowercase())
            .map_or(HashSet::new(), |r| {
                r.iter().map(|r| r.id.as_str()).collect()
            });
        let mut r = Reader::new(text, &p.name, &inputs[0].origin);
        loop {
            match r.event()? {
                Event::Eof => break,
                Event::Start(e) | Event::Empty(e) => {
                    for (ns, local, value) in r.attributes(&e)? {
                        if ns == R && !value.is_empty() && !ids.contains(value.as_str()) {
                            return internal(format!(
                                "r:{local}=\"{value}\" in {} has no relationship",
                                p.name
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// A temp file beside the output, removed unless it was renamed into place.
struct Temp(Option<PathBuf>);

impl Drop for Temp {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Counts what goes through it, refusing a part of 2 GiB or more.
struct Counted<'a, W: Write> {
    inner: &'a mut W,
    n: u64,
}

impl<W: Write> Write for Counted<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.n += buf.len() as u64;
        if self.n > MAX_OUTPUT {
            return Err(std::io::Error::other("part over 2 GiB"));
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn too_big(what: &str, n: u64) -> Error {
    Error::limit_exceeded(what, &mib(n), "2,047 MiB")
}

/// Checks the plan, writes it to a temp file in `path`'s folder and renames that into place.
/// Returns the output's size.
pub fn write(plan: Plan, inputs: &mut [Package], path: &Path) -> Result<u64, Error> {
    check(&plan, inputs)?;
    let shown = path.to_string_lossy();
    let failed = |e: std::io::Error| Error::write_failed(&shown, &e);
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp_path = dir.join(format!(
        ".{file_name}.{}-{}.tmp",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let file = File::create_new(&temp_path).map_err(failed)?;
    let mut temp = Temp(Some(temp_path));
    let mut zip = ZipWriter::new(BufWriter::with_capacity(BUFFER, file));
    let options = |time: Option<DateTime>| {
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(time.unwrap_or(DateTime::DEFAULT))
            .system(System::Dos)
    };
    let zip_failed = |e: zip::result::ZipError| match e {
        zip::result::ZipError::Io(io) => failed(io),
        e => failed(std::io::Error::other(e)),
    };
    zip.start_file(CONTENT_TYPES, options(plan.types_time))
        .map_err(zip_failed)?;
    zip.write_all(&content_types_xml(&plan.types))
        .map_err(failed)?;
    for part in plan.parts {
        inputs[0].origin.run.on_time()?;
        match part.body {
            Body::Raw { input, entry } => {
                let pkg = &mut inputs[input];
                let e = pkg.entries[entry].clone();
                if e.size > MAX_OUTPUT || e.compressed > MAX_OUTPUT {
                    return Err(too_big(
                        &format!("Part {} of the output", part.name),
                        e.size,
                    ));
                }
                let raw = pkg.raw(&e)?;
                zip.raw_copy_file_rename(raw, &part.name)
                    .map_err(zip_failed)?;
            }
            body => {
                zip.start_file(&part.name, options(part.time))
                    .map_err(zip_failed)?;
                let mut out = BufWriter::with_capacity(
                    BUFFER,
                    Counted {
                        inner: &mut zip,
                        n: 0,
                    },
                );
                let written = match body {
                    Body::Data(bytes) => out.write_all(&bytes).map_err(failed),
                    Body::Stream(stream) => stream(&mut out, inputs),
                    Body::Raw { .. } => unreachable!(),
                };
                let flushed = out.flush();
                let n = out.get_ref().n;
                if n > MAX_OUTPUT {
                    return Err(too_big(&format!("Part {} of the output", part.name), n));
                }
                written?;
                flushed.map_err(failed)?;
            }
        }
    }
    let mut file = zip.finish().map_err(zip_failed)?;
    file.flush().map_err(failed)?;
    let file = file.into_inner().map_err(|e| failed(e.into_error()))?;
    let size = file.metadata().map_err(failed)?.len();
    drop(file);
    if size > MAX_OUTPUT {
        return Err(too_big("The merged file", size));
    }
    let temp_path = temp.0.take().unwrap();
    if let Err(e) = std::fs::rename(&temp_path, path) {
        temp.0 = Some(temp_path);
        return Err(failed(e));
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_take_the_lowest_free_number() {
        let mut p = Plan::default();
        for n in [
            "ppt/slides/slide1.xml",
            "ppt/slides/Slide3.xml",
            "word/header.xml",
            "word/media/image1.png",
        ] {
            p.take(n);
        }
        assert_eq!(p.claim("ppt/slides/slide1.xml"), "ppt/slides/slide2.xml");
        assert_eq!(p.claim("ppt/slides/slide9.xml"), "ppt/slides/slide4.xml");
        assert_eq!(p.claim("PPT/slides/slide1.xml"), "PPT/slides/slide5.xml");
        assert_eq!(p.claim("word/header.xml"), "word/header1.xml");
        assert_eq!(p.claim("word/footer.xml"), "word/footer.xml");
        assert_eq!(p.claim("word/footer.xml"), "word/footer1.xml");
        assert_eq!(p.claim("word/media/image5.png"), "word/media/image2.png");
        assert_eq!(p.claim("word/media/IMAGE5.PNG"), "word/media/IMAGE3.PNG");
        assert_eq!(p.claim("customXml/item1.xml"), "customXml/item1.xml");
        assert_eq!(p.claim("noext"), "noext");
        assert_eq!(p.claim("noext"), "noext1");
    }

    #[test]
    fn targets_are_relative_opc_references() {
        assert_eq!(
            relative("word/document.xml", "word/media/image1.png"),
            "media/image1.png"
        );
        assert_eq!(
            relative("ppt/slides/slide1.xml", "ppt/slideLayouts/slideLayout1.xml"),
            "../slideLayouts/slideLayout1.xml"
        );
        assert_eq!(relative("", "word/document.xml"), "word/document.xml");
        assert_eq!(relative("a/b/c.xml", "d.xml"), "../../d.xml");
        assert_eq!(relative("a/b.xml", "a/b.xml"), "b.xml");
        assert_eq!(
            relative("w/d.xml", "w/media/a b é.png"),
            "media/a%20b%20%C3%A9.png"
        );
        for (from, to) in [
            ("ppt/slides/slide1.xml", "ppt/media/x.png"),
            ("", "a/b.xml"),
            ("a/b/c.xml", "a/d/e.xml"),
        ] {
            assert_eq!(resolve(from, &relative(from, to)).as_deref(), Some(to));
        }
        assert_eq!(
            source_of("word/_rels/document.xml.rels").as_deref(),
            Some("word/document.xml")
        );
        assert_eq!(source_of("_rels/.rels").as_deref(), Some(""));
        assert_eq!(source_of("word/document.xml"), None);
    }

    #[test]
    fn new_parts_are_typed_by_default_or_override() {
        let mut p = Plan::default();
        p.types.defaults.push(("png".into(), "image/png".into()));
        p.type_part("w/media/a.png", "image/png", true);
        p.type_part("w/media/b.emf", "image/x-emf", true);
        p.type_part("w/media/c.emf", "image/x-emf", true);
        p.type_part(
            "w/chart1.xml",
            "application/vnd.openxmlformats-officedocument.drawingml.chart+xml",
            false,
        );
        p.type_part("w/media/d.png", "image/jpeg", true);
        assert_eq!(p.types.defaults.len(), 2);
        let overrides: Vec<&str> = p.types.overrides.iter().map(|o| o.0.as_str()).collect();
        assert_eq!(overrides, ["w/chart1.xml", "w/media/d.png"]);
        assert_eq!(p.types.of("w/media/c.emf"), Some("image/x-emf"));
    }
}
