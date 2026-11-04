//! Excel: what a sheet brings along that points back at it or at the workbook: drawings,
//! charts, form controls, notes and threaded comments, and the people who wrote them.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::rc::Rc;

use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, Event};

use super::{Held, Maps, Patcher, adopt, element_prefix, io, new_part, parts_of, unreadable};
use crate::Origin;
use crate::error::Error;
use crate::package::{MAX_PARSED, MAX_STREAMED, Package, resolve};
use crate::writer::{Body, Plan, Rels};
use crate::xml::{self, Child, Node, Prefixes, Reader, declaration, start_tag};

const VML: &str = "application/vnd.openxmlformats-officedocument.vmlDrawing";
const DRAWING: &str = "application/vnd.openxmlformats-officedocument.drawing+xml";
const CHART_SHAPES: &str =
    "application/vnd.openxmlformats-officedocument.drawingml.chartshapes+xml";
const CHART: &str = "application/vnd.openxmlformats-officedocument.drawingml.chart+xml";
const CHART_EX: &str = "application/vnd.ms-office.chartex+xml";
const CONTROL: &str = "application/vnd.ms-excel.controlproperties+xml";
const NOTES: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml";
const THREADS: &str = "application/vnd.ms-excel.threadedcomments+xml";
const PEOPLE: &str = "application/vnd.ms-excel.person+xml";
const PERSON_REL: &str = "http://schemas.microsoft.com/office/2017/10/relationships/person";
const THREADS_NS: &str = "http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments";
/// The namespaces whose `f` elements hold a chart's formulas.
const CHART_NS: [&str; 3] = [
    "http://schemas.openxmlformats.org/drawingml/2006/chart",
    "http://schemas.microsoft.com/office/drawing/2012/chart",
    "http://schemas.microsoft.com/office/drawing/2014/chartex",
];
const SHAPE_PREFIX: &str = "_x0000_s";
/// VML shape ids come in blocks of this many, each owned by one drawing.
const BLOCK: u32 = 1024;

/// The ids the output's parts use so far.
pub struct Taken {
    /// VML id blocks.
    blocks: HashSet<u32>,
    /// Threaded comment and mention ids, without braces, in upper case.
    threads: HashSet<String>,
}

/// What one appended sheet's own parts turned into.
#[derive(Default)]
pub struct Moved {
    /// The parts it reaches.
    parts: Vec<String>,
    /// VML id blocks, old to new.
    blocks: HashMap<u32, u32>,
    /// Threaded comment and mention ids that would repeat, old to new, spelled as in `Taken`.
    threads: HashMap<String, String>,
}

impl Moved {
    /// Where a VML shape id moved to.
    pub fn shape(&self, id: u32) -> u32 {
        match self.blocks.get(&(id / BLOCK)) {
            Some(block) => block * BLOCK + id % BLOCK,
            None => id,
        }
    }
}

/// The parts `sheet` reaches through its relationships, each once.
fn reached(pkg: &mut Package, sheet: &str) -> Result<Vec<String>, Error> {
    let mut seen = HashSet::from([sheet.to_ascii_lowercase()]);
    let mut pending = vec![sheet.to_string()];
    let mut found = vec![];
    while let Some(part) = pending.pop() {
        for r in pkg.rels_of(&part)?.iter().filter(|r| !r.external) {
            let target = resolve(&part, &r.target).and_then(|t| pkg.find(&t));
            let Some(target) = target.map(|e| e.name.clone()) else {
                continue;
            };
            if seen.insert(target.to_ascii_lowercase()) {
                pending.push(target.clone());
                found.push(target);
            }
        }
    }
    Ok(found)
}

/// The comment and mention ids of a threaded comments part, spelled as in [`Taken`].
fn thread_ids(text: &str, part: &str, origin: &Origin) -> Result<Vec<String>, Error> {
    let mut r = Reader::new(text, part, origin);
    let mut ids = vec![];
    loop {
        let event = r.event()?;
        let (Event::Start(e) | Event::Empty(e)) = &event else {
            if matches!(event, Event::Eof) {
                return Ok(ids);
            }
            continue;
        };
        let wanted = match e.local_name().as_ref() {
            "threadedComment" => "id",
            "mention" => "mentionId",
            _ => continue,
        };
        let attrs = r.attrs(e)?;
        let id = attrs.iter().find(|a| a.ns.is_empty() && a.local == wanted);
        let id = id.map(|a| a.value.trim_matches(['{', '}']).to_ascii_uppercase());
        ids.extend(id);
    }
}

/// A GUID without braces that depends only on `seed`, so two runs write the same file.
fn guid(seed: impl Hash) -> String {
    let half = |n: u8| {
        let mut h = DefaultHasher::new();
        (n, &seed).hash(&mut h);
        h.finish()
    };
    let (a, b) = (half(0), half(1));
    format!(
        "{:08X}-{:04X}-4{:03X}-{:04X}-{:012X}",
        a >> 32,
        (a >> 16) & 0xFFFF,
        a & 0xFFF,
        (b >> 48) & 0x3FFF | 0x8000,
        b & 0xFFFF_FFFF_FFFF
    )
}

/// The text of `data="…"` in the attributes of an `o:idmap`, and where it stands.
fn idmap_data(tag: &str) -> Option<std::ops::Range<usize>> {
    let start = tag.find(" data=")? + 6;
    let quote = tag[start..].chars().next()?;
    let inner = start + quote.len_utf8();
    Some(inner..inner + tag[inner..].find(quote)?)
}

/// Every id block a VML part names or uses.
fn blocks_of(text: &str) -> BTreeSet<u32> {
    let ids = text.split(SHAPE_PREFIX).skip(1).filter_map(|s| {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse::<u32>().ok()
    });
    let mut blocks: BTreeSet<u32> = ids.map(|id| id / BLOCK).collect();
    for tag in text.split("<o:idmap").skip(1) {
        let tag = tag.split('>').next().unwrap_or(tag);
        if let Some(data) = idmap_data(tag) {
            blocks.extend(
                tag[data]
                    .split(',')
                    .filter_map(|b| b.trim().parse::<u32>().ok()),
            );
        }
    }
    blocks
}

impl Taken {
    /// What the base's parts use.
    pub fn of(pkg: &mut Package) -> Result<Self, Error> {
        let mut blocks = HashSet::new();
        for part in parts_of(pkg, VML) {
            let bytes = pkg.read(&part, MAX_PARSED)?;
            blocks.extend(blocks_of(&String::from_utf8_lossy(&bytes)));
        }
        let mut threads = HashSet::new();
        for part in parts_of(pkg, THREADS) {
            let text = pkg.text(&part, MAX_PARSED)?;
            threads.extend(thread_ids(&text, &part, &pkg.origin)?);
        }
        Ok(Self { blocks, threads })
    }

    /// Settles what the parts sheet `sheet` reaches turn into. A VML block stays when no VML
    /// part in the output uses it, and so does a threaded comment id: Excel refuses a workbook
    /// with one twice.
    pub fn add(&mut self, pkg: &mut Package, sheet: &str) -> Result<Moved, Error> {
        let mut moved = Moved {
            parts: reached(pkg, sheet)?,
            ..Moved::default()
        };
        for part in &moved.parts {
            match pkg.types.of(part) {
                Some(VML) => {
                    for old in blocks_of(&pkg.text(part, MAX_PARSED)?) {
                        moved.blocks.entry(old).or_insert_with(|| self.block(old));
                    }
                }
                Some(THREADS) => {
                    let text = pkg.text(part, MAX_PARSED)?;
                    for old in thread_ids(&text, part, &pkg.origin)? {
                        if !self.threads.insert(old.clone()) {
                            let new = self.thread(pkg.origin.input, &old);
                            moved.threads.insert(old, new);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(moved)
    }

    /// `old`, or the lowest block no VML part uses when one does.
    fn block(&mut self, old: u32) -> u32 {
        if self.blocks.insert(old) {
            return old;
        }
        let free = (1..).find(|b| !self.blocks.contains(b));
        let free = free.expect("a free block");
        self.blocks.insert(free);
        free
    }

    /// An id for a thread whose id `old` is taken.
    fn thread(&mut self, input: usize, old: &str) -> String {
        let free = (0u32..).find_map(|attempt| {
            let new = guid((input, old, attempt));
            self.threads.insert(new.clone()).then_some(new)
        });
        free.expect("a free id")
    }
}

/// The output's people: the base's, then those of appended workbooks with an id it lacks.
#[derive(Default)]
pub struct Persons {
    /// The base's part with its text.
    part: Option<(String, String)>,
    tree: Option<Node>,
    known: HashSet<String>,
    changed: bool,
}

fn person_id(person: &Node) -> String {
    person
        .attr("", "id")
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// The part with a workbook's people, read whole.
fn people(pkg: &mut Package) -> Result<Option<Held>, Error> {
    let Some(part) = parts_of(pkg, PEOPLE).into_iter().next() else {
        return Ok(None);
    };
    let text = pkg.text(&part, MAX_PARSED)?;
    let tree = xml::tree(&text, &part, &pkg.origin)?;
    Ok(Some(Held { part, text, tree }))
}

impl Persons {
    pub fn of(pkg: &mut Package) -> Result<Self, Error> {
        Ok(people(pkg)?.map_or(Self::default(), |held| Self {
            known: held.tree.nodes().map(person_id).collect(),
            part: Some((held.part, held.text)),
            tree: Some(held.tree),
            changed: false,
        }))
    }

    /// Takes in an appended workbook's people.
    pub fn add(&mut self, pkg: &mut Package) -> Result<(), Error> {
        let Some(held) = people(pkg)? else {
            return Ok(());
        };
        let Some(out) = &mut self.tree else {
            self.known = held.tree.nodes().map(person_id).collect();
            self.tree = Some(held.tree);
            self.changed = true;
            return Ok(());
        };
        let mut root = Prefixes::of(&out.attrs);
        let local = root.merge(&held.tree.attrs);
        out.attrs = root.attrs;
        let own = element_prefix(&out.name).to_string();
        for person in held.tree.nodes() {
            if self.known.insert(person_id(person)) {
                let person = adopt(person, THREADS_NS, &own, &local);
                out.children.push(Child::Node(person));
                self.changed = true;
            }
        }
        Ok(())
    }

    pub fn finish(self, plan: &mut Plan, rels: &mut Rels, main: &str) {
        let Some(tree) = self.tree.filter(|_| self.changed) else {
            return;
        };
        let head = self.part.as_ref().map(|(_, text)| declaration(text));
        let mut text = head.unwrap_or(super::DECLARATION.to_string());
        tree.write(&mut text);
        match self.part {
            Some((part, _)) => plan.replace(&part, Body::Data(text.into_bytes())),
            None => {
                let name = "xl/persons/person.xml";
                new_part(plan, rels, main, name, PERSON_REL, PEOPLE, text);
            }
        }
    }
}

/// Plans the rewrite of the parts sheet `index` of `inputs[i]` reaches, leaving those nothing
/// changes in as stored.
pub fn rewrite(plan: &mut Plan, inputs: &[Package], i: usize, index: usize, maps: &Rc<Maps>) {
    let moved = &maps.moved[index];
    let formulas = !maps.renames.are_empty();
    let shapes = moved.blocks.iter().any(|(old, new)| old != new);
    for part in &moved.parts {
        let kind = inputs[i].types.of(part).unwrap_or_default();
        let wanted = match kind {
            VML | DRAWING => formulas || shapes,
            CHART | CHART_EX | CHART_SHAPES | CONTROL => formulas,
            NOTES | THREADS => !moved.threads.is_empty(),
            _ => false,
        };
        let Some(copied) = plan.copied(i, part).filter(|_| wanted).map(str::to_string) else {
            continue;
        };
        let edit: Option<Edit> = match kind {
            VML => Some(|job, text| job.vml(text)),
            NOTES | THREADS => Some(|job, text| job.threads(text)),
            _ => None,
        };
        let maps = maps.clone();
        let source = part.clone();
        plan.replace(
            &copied,
            Body::Stream(Box::new(move |out, inputs| {
                let origin = inputs[i].origin.clone();
                let text = inputs[i].text(&source, MAX_STREAMED)?;
                let job = Job {
                    maps: &maps,
                    index,
                    part: &source,
                    origin: &origin,
                };
                match edit {
                    Some(edit) => out.write_all(edit(&job, &text)?.as_bytes()).map_err(io),
                    None => job.xml(&text, out),
                }
            })),
        );
    }
}

/// A rewrite of a whole part as text.
type Edit = fn(&Job, &str) -> Result<String, Error>;

/// `text` with what follows each `needle`, up to where `stop` holds, replaced by what `new`
/// gives, if anything.
fn edit_after(
    text: &str,
    needle: &str,
    stop: fn(char) -> bool,
    new: &mut dyn FnMut(&str) -> Result<Option<String>, Error>,
) -> Result<String, Error> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(needle) {
        let (head, tail) = rest.split_at(at + needle.len());
        out.push_str(head);
        let end = tail.find(stop).unwrap_or(tail.len());
        match new(&tail[..end])? {
            Some(new) => out.push_str(&new),
            None => out.push_str(&tail[..end]),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// One copied part of an appended sheet on its way out.
pub struct Job<'a> {
    pub maps: &'a Maps,
    pub index: usize,
    pub part: &'a str,
    pub origin: &'a Origin,
}

impl Job<'_> {
    fn moved(&self) -> &Moved {
        &self.maps.moved[self.index]
    }

    /// `f` with the workbook's renames followed; refused when it can't be read.
    fn formula<'f>(&self, f: &'f str) -> Result<Cow<'f, str>, Error> {
        let local = &self.maps.locals[self.index];
        let new = self.maps.renames.rewrite(f, local);
        new.ok_or_else(|| unreadable(self.origin, self.part))
    }

    /// The start tag of `e` with its shape ids and formulas followed, when they change.
    pub fn tag(
        &self,
        r: &Reader,
        e: &BytesStart,
        empty: bool,
        element: &str,
    ) -> Result<Option<String>, Error> {
        let mut attrs = r.attrs(e)?;
        let mut changed = false;
        for a in attrs.iter_mut().filter(|a| a.ns.is_empty()) {
            let new = match (element, a.local.as_str()) {
                ("cNvPr", "id") | ("control" | "oleObject", "shapeId") => {
                    let id = a.value.parse().ok();
                    id.map(|id| self.moved().shape(id).to_string())
                }
                ("compatExt", "spid") => {
                    let id = a.value.strip_prefix(SHAPE_PREFIX);
                    let id = id.and_then(|id| id.parse().ok());
                    id.map(|id| format!("{SHAPE_PREFIX}{}", self.moved().shape(id)))
                }
                ("sp", "textlink")
                | ("formControlPr", "fmlaLink" | "fmlaRange" | "fmlaGroup" | "fmlaTxbx")
                | ("controlPr", "linkedCell" | "listFillRange")
                    if !a.value.is_empty() =>
                {
                    Some(self.formula(&a.value)?.into_owned())
                }
                // A link to an object in another file isn't always a formula.
                ("oleObject", "link") => {
                    let local = &self.maps.locals[self.index];
                    let new = self.maps.renames.rewrite(&a.value, local);
                    new.map(Cow::into_owned)
                }
                _ => None,
            };
            if let Some(new) = new.filter(|n| *n != a.value) {
                a.value = new;
                changed = true;
            }
        }
        Ok(changed.then(|| start_tag(e.name().as_ref(), &attrs, empty)))
    }

    /// A chart, drawing or control part with its formulas and shape ids followed.
    fn xml(&self, text: &str, out: &mut dyn Write) -> Result<(), Error> {
        let mut r = Reader::new(text, self.part, self.origin);
        let mut p = Patcher {
            text,
            copied: 0,
            out,
        };
        loop {
            let start = r.position();
            let event = r.event()?;
            let end = r.position();
            let (Event::Start(e) | Event::Empty(e)) = &event else {
                if matches!(event, Event::Eof) {
                    return p.finish();
                }
                continue;
            };
            let empty = matches!(event, Event::Empty(_));
            match e.local_name().as_ref() {
                "f" if !empty && CHART_NS.contains(&r.element_ns(e)) => {
                    let (f, span) = r.element_text()?;
                    if let Cow::Owned(new) = self.formula(&f)? {
                        p.replace(span, &xml::escape_text(&new))?;
                    }
                }
                element @ ("cNvPr" | "compatExt" | "sp" | "formControlPr") => {
                    if let Some(tag) = self.tag(&r, e, empty, element)? {
                        p.replace(start..end, &tag)?;
                    }
                }
                _ => {}
            }
        }
    }

    /// A notes or threaded comments part with its repeated thread ids replaced: a note holds
    /// its thread's id as `tc={id}` author and `xr:uid`.
    fn threads(&self, text: &str) -> Result<String, Error> {
        edit_after(
            text,
            "{",
            |c| !(c.is_ascii_hexdigit() || c == '-'),
            &mut |id| {
                let new = self.moved().threads.get(&id.to_ascii_uppercase());
                Ok(new.cloned())
            },
        )
    }

    /// A VML part with its shape ids and formulas followed. VML is edited as text: Excel
    /// writes a bare `<br>` in it, so it needn't be well-formed.
    fn vml(&self, text: &str) -> Result<String, Error> {
        let moved = self.moved();
        let mut shape = |id: &str| {
            let new = id.parse().ok().filter(|n| moved.shape(*n) != *n);
            Ok(new.map(|n| moved.shape(n).to_string()))
        };
        let text = edit_after(text, SHAPE_PREFIX, |c| !c.is_ascii_digit(), &mut shape)?;
        let text = edit_after(&text, "spidmax=\"", |c| c == '"', &mut shape)?;
        let text = edit_after(&text, "<o:idmap", |c| c == '>', &mut |tag| {
            let Some(data) = idmap_data(tag) else {
                return Ok(None);
            };
            let list: Vec<String> = tag[data.clone()]
                .split(',')
                .map(
                    |b| match b.trim().parse().ok().and_then(|b| moved.blocks.get(&b)) {
                        Some(new) => new.to_string(),
                        None => b.to_string(),
                    },
                )
                .collect();
            let (head, tail) = (&tag[..data.start], &tag[data.end..]);
            Ok(Some(format!("{head}{}{tail}", list.join(","))))
        })?;
        edit_after(&text, "<x:Fmla", |c| c == '<', &mut |s| {
            let Some((kind, f)) = s.split_once('>').filter(|(kind, _)| *kind != "Macro") else {
                return Ok(None);
            };
            let plain = unescape(f).unwrap_or(Cow::Borrowed(f));
            Ok(match self.formula(&plain)? {
                Cow::Owned(new) => Some(format!("{kind}>{}", xml::escape_text(&new))),
                Cow::Borrowed(_) => None,
            })
        })
    }
}
