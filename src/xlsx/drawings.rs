//! Excel: what a sheet brings along that points back at it or at the workbook: drawings,
//! charts and form controls.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::rc::Rc;

use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, Event};

use super::{Maps, Patcher, io, parts_of, unreadable};
use crate::Origin;
use crate::error::Error;
use crate::package::{MAX_PARSED, MAX_STREAMED, Package, resolve};
use crate::writer::{Body, Plan};
use crate::xml::{self, Reader, start_tag};

const VML: &str = "application/vnd.openxmlformats-officedocument.vmlDrawing";
const DRAWING: &str = "application/vnd.openxmlformats-officedocument.drawing+xml";
const CHART_SHAPES: &str =
    "application/vnd.openxmlformats-officedocument.drawingml.chartshapes+xml";
const CHART: &str = "application/vnd.openxmlformats-officedocument.drawingml.chart+xml";
const CHART_EX: &str = "application/vnd.ms-office.chartex+xml";
const CONTROL: &str = "application/vnd.ms-excel.controlproperties+xml";
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
}

/// What one appended sheet's own parts turned into.
#[derive(Default)]
pub struct Moved {
    /// The parts it reaches.
    parts: Vec<String>,
    /// VML id blocks, old to new.
    blocks: HashMap<u32, u32>,
}

impl Moved {
    /// Where a VML shape id moved to.
    pub fn shape(&self, id: u32) -> u32 {
        match self.blocks.get(&(id / BLOCK)) {
            Some(block) => block * BLOCK + id % BLOCK,
            None => id,
        }
    }

    fn shapes_moved(&self) -> bool {
        self.blocks.iter().any(|(old, new)| old != new)
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
        Ok(Self { blocks })
    }

    /// Settles what the parts sheet `sheet` reaches turn into. A VML block stays when no VML
    /// part in the output uses it.
    pub fn add(&mut self, pkg: &mut Package, sheet: &str) -> Result<Moved, Error> {
        let mut moved = Moved {
            parts: reached(pkg, sheet)?,
            ..Moved::default()
        };
        for part in &moved.parts {
            if pkg.types.of(part) != Some(VML) {
                continue;
            }
            for old in blocks_of(&pkg.text(part, MAX_PARSED)?) {
                if moved.blocks.contains_key(&old) {
                    continue;
                }
                let new = if self.blocks.insert(old) {
                    old
                } else {
                    let free = (1..).find(|b| !self.blocks.contains(b));
                    let free = free.expect("a free block");
                    self.blocks.insert(free);
                    free
                };
                moved.blocks.insert(old, new);
            }
        }
        Ok(moved)
    }
}

/// Plans the rewrite of the parts sheet `index` of `inputs[i]` reaches, leaving those nothing
/// changes in as stored.
pub fn rewrite(plan: &mut Plan, inputs: &[Package], i: usize, index: usize, maps: &Rc<Maps>) {
    let moved = &maps.moved[index];
    let formulas = !maps.renames.are_empty();
    let shapes = moved.shapes_moved();
    for part in &moved.parts {
        let kind = inputs[i].types.of(part).unwrap_or_default();
        let wanted = match kind {
            VML | DRAWING => formulas || shapes,
            CHART | CHART_EX | CHART_SHAPES | CONTROL => formulas,
            _ => false,
        };
        let Some(copied) = plan.copied(i, part).filter(|_| wanted).map(str::to_string) else {
            continue;
        };
        let vml = kind == VML;
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
                if vml {
                    out.write_all(job.vml(&text)?.as_bytes()).map_err(io)
                } else {
                    job.xml(&text, out)
                }
            })),
        );
    }
}

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

/// The attributes of the elements that hold shape ids or formulas.
enum Attribute {
    ShapeId,
    /// `_x0000_s` and a shape id.
    ShapeName,
    Formula,
    /// A link to an object in another file, not always a formula.
    ObjectLink,
}

fn attribute(element: &str, name: &str) -> Option<Attribute> {
    match (element, name) {
        ("cNvPr", "id") | ("control" | "oleObject", "shapeId") => Some(Attribute::ShapeId),
        ("compatExt", "spid") => Some(Attribute::ShapeName),
        ("sp", "textlink")
        | ("formControlPr", "fmlaLink" | "fmlaRange" | "fmlaGroup" | "fmlaTxbx")
        | ("controlPr", "linkedCell" | "listFillRange") => Some(Attribute::Formula),
        ("oleObject", "link") => Some(Attribute::ObjectLink),
        _ => None,
    }
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
            let new = match attribute(element, &a.local) {
                Some(Attribute::ShapeId) => a
                    .value
                    .parse()
                    .ok()
                    .map(|id| self.moved().shape(id).to_string()),
                Some(Attribute::ShapeName) => a
                    .value
                    .strip_prefix(SHAPE_PREFIX)
                    .and_then(|id| id.parse().ok())
                    .map(|id| format!("{SHAPE_PREFIX}{}", self.moved().shape(id))),
                Some(Attribute::Formula) if !a.value.is_empty() => {
                    Some(self.formula(&a.value)?.into_owned())
                }
                Some(Attribute::ObjectLink) => {
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
