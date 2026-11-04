//! Excel: every workbook's sheets after the base's, each streamed with its indices and
//! formulas rewritten, under names that stay unique.

pub mod a1;

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use quick_xml::events::{BytesStart, Event};

use crate::error::{Error, Warnings, thousands};
use crate::package::{MAX_PARSED, MAX_STREAMED, Package, Rel, resolve};
use crate::writer::{Body, Plan, R, Rels, relative, rels_part};
use crate::xml::{self, Attr, Node, Reader, declaration, prefix_of, start_tag};
use crate::{Origin, Rename, SheetName};

pub const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const SML: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.";
const MAX_SHEETS: usize = 10_000;
/// Excel's limit, counted in UTF-16 units.
const MAX_SHEET_NAME: usize = 31;

/// What a merge of workbooks gives besides the plan.
pub struct Merged {
    pub plan: Plan,
    /// Each input's sheets, in order.
    pub sheets: Vec<Vec<SheetName>>,
    pub renames: Vec<Rename>,
}

pub fn merge(inputs: &mut [Package], warnings: &mut Warnings) -> Result<Merged, Error> {
    let mut plan = Plan::base(&inputs[0], warnings)?;
    let base = Book::read(&mut inputs[0])?;
    let mut sheets = vec![
        base.sheets
            .iter()
            .map(|s| SheetName {
                name: s.name.clone(),
                output_name: s.name.clone(),
            })
            .collect(),
    ];
    let mut out = Output::new(base, &mut inputs[0])?;
    let mut renames = vec![];
    for i in 1..inputs.len() {
        sheets.push(out.append(&mut plan, inputs, i, &mut renames)?);
    }
    out.finish(&mut plan, &inputs[0])?;
    Ok(Merged {
        plan,
        sheets,
        renames,
    })
}

/// A sheet as its workbook lists it.
struct Sheet {
    name: String,
    part: String,
    /// Its relationship's type.
    kind: String,
    node: Node,
}

/// A workbook's main part and what it lists.
struct Book {
    part: String,
    text: String,
    tree: Node,
    rels: Vec<Rel>,
    sheets: Vec<Sheet>,
}

/// The part relationship `r` of `part` points to.
fn target(pkg: &Package, part: &str, r: &Rel) -> Result<String, Error> {
    let found = resolve(part, &r.target).and_then(|t| pkg.find(&t).map(|e| e.name.clone()));
    found.ok_or_else(|| {
        pkg.origin
            .bad(&format!("{part} points to a missing part {}", r.target))
    })
}

impl Book {
    fn read(pkg: &mut Package) -> Result<Self, Error> {
        let part = pkg.main.clone();
        let text = pkg.text(&part, MAX_PARSED)?;
        let tree = xml::tree(&text, &part, &pkg.origin)?;
        let rels = pkg.rels_of(&part)?;
        let mut sheets = vec![];
        for node in tree.child(S, "sheets").iter().flat_map(|n| n.nodes()) {
            let id = node.attr(R, "id").unwrap_or_default();
            let Some(r) = rels.iter().find(|r| r.id == id && !r.external) else {
                let why = format!("{part} points to a missing relationship {id}");
                return Err(pkg.origin.bad(&why));
            };
            sheets.push(Sheet {
                name: node.attr("", "name").unwrap_or_default().into(),
                part: target(pkg, &part, r)?,
                kind: r.kind.clone(),
                node: node.clone(),
            });
        }
        Ok(Self {
            part,
            text,
            tree,
            rels,
            sheets,
        })
    }

    /// The part this workbook relates to by a relationship type.
    fn related(&self, pkg: &Package, kind: &str) -> Result<Option<String>, Error> {
        let wanted = format!("{REL}{kind}");
        self.rels
            .iter()
            .find(|r| r.kind == wanted && !r.external)
            .map(|r| target(pkg, &self.part, r))
            .transpose()
    }
}

/// The prefix of an element's name with its colon, or nothing.
fn prefix(node: &Node) -> &str {
    &node.name[..node.name.len() - node.local.len()]
}

/// The names of the output's sheets, which differ ignoring case.
#[derive(Default)]
struct SheetNames {
    taken: HashSet<String>,
    /// Per name, the number to try next.
    next: HashMap<String, u32>,
}

impl SheetNames {
    /// `name`, or `{name} ({k})` with `name` cut to fit, when it is taken.
    fn unique(&mut self, name: &str) -> String {
        let key = name.to_lowercase();
        if self.taken.insert(key.clone()) {
            return name.into();
        }
        let mut k = self.next.get(&key).copied().unwrap_or(2);
        loop {
            let suffix = format!(" ({k})");
            let mut room = MAX_SHEET_NAME - suffix.len();
            let kept: String = name
                .chars()
                .take_while(|c| {
                    let fits = c.len_utf16() <= room;
                    room = room.saturating_sub(c.len_utf16());
                    fits
                })
                .collect();
            let candidate = kept + &suffix;
            k += 1;
            if self.taken.insert(candidate.to_lowercase()) {
                self.next.insert(key, k);
                return candidate;
            }
        }
    }
}

/// The items of a shared-strings part.
struct Strings<'t> {
    /// `si` elements as written.
    items: Vec<&'t str>,
    root: Vec<Attr>,
    name: String,
    /// The root's start tag, and the text between it and the root's end.
    tag: Range<usize>,
    inner: Range<usize>,
}

fn strings<'t>(text: &'t str, part: &'t str, origin: &Origin) -> Result<Strings<'t>, Error> {
    let mut r = Reader::new(text, part, origin);
    let mut found = Strings {
        items: vec![],
        root: vec![],
        name: String::new(),
        tag: 0..0,
        inner: 0..0,
    };
    let mut item = 0;
    loop {
        let start = r.position();
        let event = r.event()?;
        let end = r.position();
        match &event {
            Event::Eof => return Ok(found),
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                match r.depth() - usize::from(!empty) {
                    0 => {
                        found.root = r.attrs(e)?;
                        found.name = e.name().as_ref().to_owned();
                        found.tag = start..end;
                        found.inner = end..end;
                    }
                    1 if empty => found.items.push(&text[start..end]),
                    1 => item = start,
                    _ => {}
                }
            }
            Event::End(_) => match r.depth() {
                0 => found.inner.end = start,
                1 => found.items.push(&text[item..end]),
                _ => {}
            },
            _ => {}
        }
    }
}

/// XML text with the prefix of its elements changed (each with its colon, or empty).
fn with_prefix(xml: &str, from: &str, to: &str) -> String {
    xml.replace(&format!("</{from}"), "\0")
        .replace(&format!("<{from}"), &format!("<{to}"))
        .replace('\0', &format!("</{to}"))
}

/// The output's shared strings: the base's, then those of appended workbooks that differ.
struct SharedStrings {
    part: Option<String>,
    text: String,
    /// The prefix its elements are written with.
    prefix: String,
    index: HashMap<String, u32>,
    /// How many items it holds, some perhaps equal, and the appended ones as text.
    items: u32,
    added: String,
    /// How many cells use them, and whether that changed.
    count: u64,
    changed: bool,
}

impl SharedStrings {
    fn new(pkg: &mut Package, part: Option<String>) -> Result<Self, Error> {
        let mut out = Self {
            part,
            text: String::new(),
            prefix: String::new(),
            index: HashMap::new(),
            items: 0,
            added: String::new(),
            count: 0,
            changed: false,
        };
        if let Some(part) = &out.part {
            out.text = pkg.text(part, MAX_PARSED)?;
            let s = strings(&out.text, part, &pkg.origin)?;
            out.count = declared_count(&s);
            let prefix = element_prefix(&s.name).to_string();
            let items = s.items.len() as u32;
            let index = s.items.iter().enumerate().rev();
            let index = index
                .map(|(i, item)| (item.to_string(), i as u32))
                .collect();
            (out.prefix, out.items, out.index) = (prefix, items, index);
        }
        Ok(out)
    }

    /// Takes in an appended workbook's strings; returns each one's index in the output.
    fn add(&mut self, text: &str, part: &str, origin: &Origin) -> Result<Vec<u32>, Error> {
        let s = strings(text, part, origin)?;
        let from = element_prefix(&s.name);
        let mut map = Vec::with_capacity(s.items.len());
        for item in &s.items {
            let item = if from == self.prefix {
                item.to_string()
            } else {
                with_prefix(item, from, &self.prefix)
            };
            map.push(*self.index.entry(item).or_insert_with_key(|item| {
                self.added.push_str(item);
                self.items += 1;
                self.items - 1
            }));
        }
        self.count += declared_count(&s);
        self.changed |= !s.items.is_empty();
        Ok(map)
    }

    fn finish(
        self,
        plan: &mut Plan,
        rels: &mut Rels,
        main: &str,
        origin: &Origin,
    ) -> Result<(), Error> {
        if !self.changed {
            return Ok(());
        }
        let unique = self.items.to_string();
        let count = self.count.to_string();
        let set = |attrs: &mut Vec<Attr>| {
            for (local, value) in [("count", &count), ("uniqueCount", &unique)] {
                match attrs
                    .iter_mut()
                    .find(|a| a.ns.is_empty() && a.local == local)
                {
                    Some(a) => a.value = value.clone(),
                    None => attrs.push(Attr::new(local, "", value)),
                }
            }
        };
        let Some(part) = &self.part else {
            let mut root = vec![Attr::new("xmlns", xml::XMLNS, S)];
            root[0].local.clear();
            set(&mut root);
            let text = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n{}{}</sst>",
                start_tag("sst", &root, false),
                self.added
            );
            let part = plan.claim("xl/sharedStrings.xml");
            plan.type_part(&part, &format!("{SML}sharedStrings+xml"), false);
            rels.add(&format!("{REL}sharedStrings"), relative(main, &part), false);
            plan.add(part, Body::Data(text.into_bytes()), None);
            return Ok(());
        };
        let s = strings(&self.text, part, origin)?;
        let mut root = s.root.clone();
        set(&mut root);
        let text = format!(
            "{}{}{}{}</{}>",
            &self.text[..s.tag.start],
            start_tag(&s.name, &root, false),
            &self.text[s.inner.clone()],
            self.added,
            s.name
        );
        plan.replace(part, Body::Data(text.into_bytes()));
        Ok(())
    }
}

fn element_prefix(name: &str) -> &str {
    name.rfind(':').map_or("", |i| &name[..=i])
}

fn declared_count(s: &Strings) -> u64 {
    s.root
        .iter()
        .find(|a| a.ns.is_empty() && a.local == "count")
        .and_then(|a| a.value.parse().ok())
        .unwrap_or(s.items.len() as u64)
}

/// The merged workbook as it grows.
struct Output {
    book: Book,
    rels: Rels,
    names: SheetNames,
    sheet_count: usize,
    next_sheet_id: u64,
    /// How many views the base's `bookViews` holds.
    views: usize,
    strings: SharedStrings,
}

impl Output {
    fn new(book: Book, pkg: &mut Package) -> Result<Self, Error> {
        let mut names = SheetNames::default();
        for s in &book.sheets {
            names.taken.insert(s.name.to_lowercase());
        }
        let next_sheet_id = book
            .sheets
            .iter()
            .filter_map(|s| s.node.attr("", "sheetId")?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let views = book
            .tree
            .child(S, "bookViews")
            .map_or(0, |n| n.nodes().count());
        let strings = SharedStrings::new(pkg, book.related(pkg, "sharedStrings")?)?;
        Ok(Self {
            rels: Rels::new(book.rels.clone()),
            names,
            sheet_count: book.sheets.len(),
            next_sheet_id,
            views: views.max(1),
            strings,
            book,
        })
    }

    fn append(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        renames: &mut Vec<Rename>,
    ) -> Result<Vec<SheetName>, Error> {
        let book = Book::read(&mut inputs[i])?;
        let origin = inputs[i].origin.clone();
        self.sheet_count += book.sheets.len();
        if self.sheet_count > MAX_SHEETS {
            return Err(Error::limit_exceeded(
                "The merged workbook's sheet count",
                &thousands(self.sheet_count as u64),
                &thousands(MAX_SHEETS as u64),
            ));
        }
        let strings = match book.related(&inputs[i], "sharedStrings")? {
            Some(part) => {
                let text = inputs[i].text(&part, MAX_PARSED)?;
                self.strings.add(&text, &part, &origin)?
            }
            None => vec![],
        };
        let maps = Rc::new(Maps {
            strings,
            views: self.views,
        });
        let main = self.book.part.clone();
        let r = prefix_of(&self.book.tree.attrs, R)
            .unwrap_or("r")
            .to_string();
        let list = prefix(&self.book.tree).to_string();
        let mut listed = vec![];
        for s in book.sheets {
            let output_name = self.names.unique(&s.name);
            if output_name != s.name {
                renames.push(Rename {
                    input: i + 1,
                    kind: "sheet",
                    from: s.name.clone(),
                    to: output_name.clone(),
                });
            }
            let part = plan.copy_graph(inputs, i, &s.part, &|_, _| None)?;
            let maps = maps.clone();
            let source = s.part.clone();
            let shown = output_name.clone();
            plan.replace(
                &part,
                Body::Stream(Box::new(move |out, inputs| {
                    let origin = inputs[i].origin.clone();
                    let text = inputs[i].text(&source, MAX_STREAMED)?;
                    sheet(&text, &source, &origin, &maps, &shown, out)
                })),
            );
            let id = self.rels.add(&s.kind, relative(&main, &part), false);
            let mut node = s.node;
            node.name = format!("{list}sheet");
            node.attrs.retain(|a| a.ns != R);
            node.attrs.push(Attr::new(&format!("{r}:id"), R, &id));
            node.set_attr("", "", "name", &output_name);
            node.set_attr("", "", "sheetId", &self.next_sheet_id.to_string());
            self.next_sheet_id += 1;
            if let Some(n) = self.book.tree.child_mut(S, "sheets") {
                n.children.push(xml::Child::Node(node));
            }
            listed.push(SheetName {
                name: s.name,
                output_name,
            });
        }
        Ok(listed)
    }

    fn finish(self, plan: &mut Plan, base: &Package) -> Result<(), Error> {
        let Self {
            book,
            mut rels,
            strings,
            ..
        } = self;
        strings.finish(plan, &mut rels, &book.part, &base.origin)?;
        let mut text = declaration(&book.text);
        book.tree.write(&mut text);
        plan.replace(&book.part, Body::Data(text.into_bytes()));
        rels_part(plan, base, &book.part, &rels);
        Ok(())
    }
}

/// How one appended workbook's content changes on its way into the output.
struct Maps {
    /// Shared-string indices.
    strings: Vec<u32>,
    views: usize,
}

/// Copies a part, with chosen spans replaced.
struct Patcher<'a> {
    text: &'a str,
    copied: usize,
    out: &'a mut dyn std::io::Write,
}

fn io(e: std::io::Error) -> Error {
    Error::write_failed("the merged file", &e)
}

impl Patcher<'_> {
    fn replace(&mut self, span: Range<usize>, new: &str) -> Result<(), Error> {
        self.out
            .write_all(&self.text.as_bytes()[self.copied..span.start])
            .map_err(io)?;
        self.out.write_all(new.as_bytes()).map_err(io)?;
        self.copied = span.end;
        Ok(())
    }

    fn finish(self) -> Result<(), Error> {
        self.out
            .write_all(&self.text.as_bytes()[self.copied..])
            .map_err(io)
    }
}

/// Whether a cell holds a shared string.
fn is_string_cell(r: &Reader, e: &BytesStart) -> Result<bool, Error> {
    for a in e.attributes() {
        let a = a.map_err(|e| r.malformed(&e.to_string()))?;
        if a.key.as_ref() == "t" {
            return Ok(a.value == "s");
        }
    }
    Ok(false)
}

/// Writes an appended sheet with everything that points elsewhere pointing at the output's.
fn sheet(
    text: &str,
    part: &str,
    origin: &Origin,
    maps: &Maps,
    shown: &str,
    out: &mut dyn std::io::Write,
) -> Result<(), Error> {
    let mut r = Reader::new(text, part, origin);
    let mut p = Patcher {
        text,
        copied: 0,
        out,
    };
    let mut string_cell = false;
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
            "c" if r.element_ns(e) == S => string_cell = is_string_cell(&r, e)?,
            "v" if string_cell && !empty && r.element_ns(e) == S => {
                let (value, span) = r.element_text()?;
                let new = value
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| maps.strings.get(n));
                let Some(new) = new else {
                    let why = format!("sheet {shown} uses a shared string it doesn't have");
                    return Err(origin.bad(&why));
                };
                p.replace(span, &new.to_string())?;
            }
            "sheetView" if r.element_ns(e) == S => {
                let mut attrs = r.attrs(e)?;
                let before = attrs.len();
                attrs.retain(|a| !(a.ns.is_empty() && a.local == "tabSelected"));
                let mut changed = attrs.len() != before;
                for a in &mut attrs {
                    let beyond = a.value.parse().is_ok_and(|v: usize| v >= maps.views);
                    if a.ns.is_empty() && a.local == "workbookViewId" && beyond {
                        a.value = "0".into();
                        changed = true;
                    }
                }
                if changed {
                    p.replace(start..end, &start_tag(e.name().as_ref(), &attrs, empty))?;
                }
            }
            _ => {}
        }
    }
}
