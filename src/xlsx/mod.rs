//! Excel: every workbook's sheets after the base's, each streamed with its indices and
//! formulas rewritten, under names that stay unique.

pub mod a1;
mod styles;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use quick_xml::events::{BytesStart, Event};

use crate::error::{Error, Warning, Warnings, thousands};
use crate::package::{MAX_PARSED, MAX_STREAMED, Package, Rel, resolve};
use crate::writer::{Body, Plan, R, Rels, relative, rels_part};
use crate::xml::{self, Attr, Node, Reader, declaration, prefix_of, start_tag};
use crate::{Origin, Rename, SheetName};

pub const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const SML: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.";
const DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n";
const XM: &str = "http://schemas.microsoft.com/office/excel/2006/main";
const X14: &str = "http://schemas.microsoft.com/office/spreadsheetml/2009/9/main";
const MAX_SHEETS: usize = 10_000;
const NAME_ROUNDS: usize = 8;
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
        sheets.push(out.append(&mut plan, inputs, i, &mut renames, warnings)?);
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

/// A part read whole.
struct Held {
    part: String,
    text: String,
    tree: Node,
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

    /// A part this workbook relates to, read whole.
    fn held(&self, pkg: &mut Package, kind: &str) -> Result<Option<Held>, Error> {
        let Some(part) = self.related(pkg, kind)? else {
            return Ok(None);
        };
        let text = pkg.text(&part, MAX_PARSED)?;
        let tree = xml::tree(&text, &part, &pkg.origin)?;
        Ok(Some(Held { part, text, tree }))
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
                "{DECLARATION}{}{}</sst>",
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
    /// By lower-case name, the output's workbook-scope names with their formulas and its
    /// tables with none: the two share one namespace.
    used: HashMap<String, Option<String>>,
    next_table_id: u64,
    links: u32,
    styles: styles::Output,
    base_styles: Option<Held>,
    base_theme: Option<Held>,
}

/// A table part as read.
struct Table {
    part: String,
    text: String,
    tree: Node,
    name: String,
}

fn tables(pkg: &mut Package) -> Result<Vec<Table>, Error> {
    let kind = format!("{SML}table+xml");
    let parts: Vec<String> = pkg
        .entries
        .iter()
        .filter(|e| pkg.types.of(&e.name) == Some(kind.as_str()))
        .map(|e| e.name.clone())
        .collect();
    let mut list = vec![];
    for part in parts {
        let text = pkg.text(&part, MAX_PARSED)?;
        let tree = xml::tree(&text, &part, &pkg.origin)?;
        let name = tree
            .attr("", "displayName")
            .or(tree.attr("", "name"))
            .unwrap_or_default()
            .to_string();
        list.push(Table {
            part,
            text,
            tree,
            name,
        });
    }
    Ok(list)
}

/// A defined name as read.
struct Defined {
    node: Node,
    name: String,
    key: String,
    /// The position of the sheet it belongs to, when it isn't the workbook's.
    sheet: Option<usize>,
    formula: String,
}

fn defined_names(book: &Book, origin: &Origin) -> Result<Vec<Defined>, Error> {
    let mut list = vec![];
    for node in book
        .tree
        .child(S, "definedNames")
        .iter()
        .flat_map(|n| n.nodes())
    {
        let name = node.attr("", "name").unwrap_or_default().to_string();
        let sheet = node
            .attr("", "localSheetId")
            .map(|v| v.parse::<usize>().ok().filter(|n| *n < book.sheets.len()));
        if sheet == Some(None) {
            return Err(origin.bad(&format!(
                "the name {name} belongs to a sheet it doesn't have"
            )));
        }
        list.push(Defined {
            key: name.to_lowercase(),
            sheet: sheet.flatten(),
            formula: node.text(),
            node: node.clone(),
            name,
        });
    }
    Ok(list)
}

fn unreadable(origin: &Origin, place: &str) -> Error {
    let variant = format!("a formula it can't read ({place})");
    Error::unsupported_variant(&origin.name, &variant).at(origin.input)
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
        let links = book
            .tree
            .child(S, "externalReferences")
            .map_or(0, |n| n.nodes().count());
        let strings = SharedStrings::new(pkg, book.related(pkg, "sharedStrings")?)?;
        let base_styles = book.held(pkg, "styles")?;
        let base_theme = book.held(pkg, "theme")?;
        let mut used = HashMap::new();
        for d in defined_names(&book, &pkg.origin)? {
            if d.sheet.is_none() {
                used.insert(d.key, Some(d.formula));
            }
        }
        let mut next_table_id = 1;
        for t in tables(pkg)? {
            used.insert(t.name.to_lowercase(), None);
            let id = t.tree.attr("", "id").and_then(|v| v.parse::<u64>().ok());
            next_table_id = next_table_id.max(id.unwrap_or(0) + 1);
        }
        Ok(Self {
            rels: Rels::new(book.rels.clone()),
            names,
            sheet_count: book.sheets.len(),
            next_sheet_id,
            views: views.max(1),
            strings,
            used,
            next_table_id,
            links: links as u32,
            styles: styles::Output::new(base_styles.as_ref().map(|h| h.tree.clone())),
            base_styles,
            base_theme,
            book,
        })
    }

    /// `{name}_{k}` with the lowest `k` from 2 that neither the output nor `own` uses.
    fn free_name(&self, name: &str, own: &HashSet<String>) -> String {
        (2u64..)
            .map(|k| format!("{name}_{k}"))
            .find(|n| {
                let key = n.to_lowercase();
                !self.used.contains_key(&key) && !own.contains(&key)
            })
            .expect("a free number")
    }

    /// Settles the names of an appended workbook's tables and defined names, adding them to
    /// `found.names`, and returns the definitions the output takes.
    fn settle_names(
        &mut self,
        tables: &[Table],
        names: &[Defined],
        found: &mut Found,
        input: usize,
    ) -> Result<Vec<Node>, Error> {
        let mut own: HashSet<String> = names.iter().map(|d| d.key.clone()).collect();
        own.extend(tables.iter().map(|t| t.name.to_lowercase()));
        for t in tables {
            let key = t.name.to_lowercase();
            let mut new = t.name.clone();
            if self.used.contains_key(&key) {
                new = self.free_name(&t.name, &own);
                own.insert(new.to_lowercase());
                found.renames.names.insert(key, new.clone());
                found.note(input, "table", &t.name, &new);
            }
            self.used.insert(new.to_lowercase(), None);
        }
        let none = HashSet::new();
        let wide: Vec<&Defined> = names.iter().filter(|d| d.sheet.is_none()).collect();
        // A name equal to the output's stops being equal when a name it uses is renamed; one
        // pass per link of such a chain, up to a depth no real workbook reaches.
        for _ in 0..NAME_ROUNDS {
            let mut changed = false;
            for d in &wide {
                let Some(there) = self.used.get(&d.key) else {
                    continue;
                };
                if found.renames.names.contains_key(&d.key) {
                    continue;
                }
                let formula = found.rewrite(&d.formula, &none, &d.name)?;
                if there.as_deref() != Some(formula.as_ref()) {
                    let new = self.free_name(&d.name, &own);
                    own.insert(new.to_lowercase());
                    if d.formula.contains("LAMBDA(") {
                        let called = format!("{}(", d.key);
                        found.renames.names.insert(called, new.clone());
                    }
                    found.renames.names.insert(d.key.clone(), new.clone());
                    found.note(input, "defined_name", &d.name, &new);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let first = self.sheet_count - found.locals.len();
        let mut added = vec![];
        for d in names {
            let local = d.sheet.map_or(&none, |s| &found.locals[s]);
            let formula = found.rewrite(&d.formula, local, &d.name)?.into_owned();
            let mut node = d.node.clone();
            if let Some(s) = d.sheet {
                node.set_attr("", "", "localSheetId", &(first + s).to_string());
            } else {
                let new = found.renames.names.get(&d.key).unwrap_or(&d.name);
                if self.used.get(&new.to_lowercase()) == Some(&Some(formula.clone())) {
                    continue;
                }
                node.set_attr("", "", "name", new);
                self.used.insert(new.to_lowercase(), Some(formula.clone()));
            }
            node.children = vec![xml::Child::Text(formula)];
            added.push(node);
        }
        Ok(added)
    }

    /// Copies an appended workbook's external links; returns the number its links start after.
    fn copy_links(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        book: &Book,
    ) -> Result<u32, Error> {
        let offset = self.links;
        let main = self.book.part.clone();
        let list = book.tree.child(S, "externalReferences");
        for node in list.iter().flat_map(|n| n.nodes()) {
            let id = node.attr(R, "id").unwrap_or_default();
            let Some(r) = book.rels.iter().find(|r| r.id == id && !r.external) else {
                let why = format!("{} points to a missing relationship {id}", book.part);
                return Err(inputs[i].origin.bad(&why));
            };
            let source = target(&inputs[i], &book.part, r)?;
            let part = plan.copy_graph(inputs, i, &source, &|_, _| None)?;
            let new = self.rels.add(&r.kind, relative(&main, &part), false);
            let node = self.with_id(node.clone(), "externalReference", &new);
            let after = ["sheets", "functionGroups"];
            let list = self
                .book
                .tree
                .child_or_insert(S, "externalReferences", &after);
            list.children.push(xml::Child::Node(node));
            self.links += 1;
        }
        Ok(offset)
    }

    /// An appended workbook's list entry, spelled the way the output spells them and with the
    /// relationship id `id`.
    fn with_id(&self, mut node: Node, local: &str, id: &str) -> Node {
        let r = prefix_of(&self.book.tree.attrs, R).unwrap_or("r");
        node.name = format!("{}{local}", prefix(&self.book.tree));
        node.attrs.retain(|a| a.ns != R);
        node.attrs.push(Attr::new(&format!("{r}:id"), R, id));
        node
    }

    fn append(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        renames: &mut Vec<Rename>,
        warnings: &mut Warnings,
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
        let mut found = Found {
            renames: a1::Renames::default(),
            locals: vec![HashSet::new(); book.sheets.len()],
            list: vec![],
            origin: origin.clone(),
        };
        let mut listed = vec![];
        for s in &book.sheets {
            let output_name = self.names.unique(&s.name);
            if output_name != s.name {
                found.note(i + 1, "sheet", &s.name, &output_name);
                let key = s.name.to_lowercase();
                found.renames.sheets.insert(key, output_name.clone());
            }
            listed.push(SheetName {
                name: s.name.clone(),
                output_name,
            });
        }
        let names = defined_names(&book, &origin)?;
        for d in &names {
            if let Some(s) = d.sheet {
                found.locals[s].insert(d.key.clone());
            }
        }
        found.renames.links = self.copy_links(plan, inputs, i, &book)?;
        let tables = tables(&mut inputs[i])?;
        let added = self.settle_names(&tables, &names, &mut found, i + 1)?;
        if !added.is_empty() {
            let after = ["sheets", "functionGroups", "externalReferences"];
            let list = self.book.tree.child_or_insert(S, "definedNames", &after);
            let own = prefix(list).to_string();
            list.children.extend(added.into_iter().map(|mut n| {
                n.name = format!("{own}definedName");
                xml::Child::Node(n)
            }));
        }
        let strings = match book.related(&inputs[i], "sharedStrings")? {
            Some(part) => {
                let text = inputs[i].text(&part, MAX_PARSED)?;
                self.strings.add(&text, &part, &origin)?
            }
            None => vec![],
        };
        let sheet = book.held(&mut inputs[i], "styles")?;
        let sheet = sheet.as_ref().map(|h| &h.tree);
        let theme = book.held(&mut inputs[i], "theme")?;
        let colours = styles::Colours::new(
            theme.as_ref().map(|h| &h.tree),
            self.base_theme.as_ref().map(|h| &h.tree),
            sheet,
            self.base_styles.as_ref().map(|h| &h.tree),
        );
        if colours.as_ref().is_some_and(styles::Colours::themed) {
            warnings.push(Warning::theme_differs(i + 1, &origin.name));
        }
        let styles = self
            .styles
            .add(sheet, colours.as_ref(), &origin, &mut found.list)?;
        renames.append(&mut found.list);
        let maps = Rc::new(Maps {
            strings,
            styles,
            colours,
            views: self.views,
            renames: found.renames,
            locals: found.locals,
        });
        let main = self.book.part.clone();
        for (index, s) in book.sheets.into_iter().enumerate() {
            let shown = listed[index].output_name.clone();
            let part = plan.copy_graph(inputs, i, &s.part, &|_, _| None)?;
            let maps = maps.clone();
            let source = s.part.clone();
            let name = shown.clone();
            plan.replace(
                &part,
                Body::Stream(Box::new(move |out, inputs| {
                    let origin = inputs[i].origin.clone();
                    let text = inputs[i].text(&source, MAX_STREAMED)?;
                    let job = SheetJob {
                        maps: &maps,
                        index,
                        name: &name,
                    };
                    job.write(&text, &source, &origin, out)
                })),
            );
            let id = self.rels.add(&s.kind, relative(&main, &part), false);
            let mut node = self.with_id(s.node, "sheet", &id);
            node.set_attr("", "", "name", &shown);
            node.set_attr("", "", "sheetId", &self.next_sheet_id.to_string());
            self.next_sheet_id += 1;
            if let Some(n) = self.book.tree.child_mut(S, "sheets") {
                n.children.push(xml::Child::Node(node));
            }
        }
        for t in tables {
            let Some(copied) = plan.copied(i, &t.part) else {
                continue;
            };
            let copied = copied.to_string();
            let body = self.table(t, &maps, &origin)?;
            plan.replace(&copied, Body::Data(body));
        }
        Ok(listed)
    }

    /// An appended table part with a new id, its renames and its formulas followed.
    fn table(&mut self, t: Table, maps: &Maps, origin: &Origin) -> Result<Vec<u8>, Error> {
        let Table {
            text,
            mut tree,
            name,
            ..
        } = t;
        tree.set_attr("", "", "id", &self.next_table_id.to_string());
        self.next_table_id += 1;
        if let Some(new) = maps.renames.names.get(&name.to_lowercase()) {
            tree.set_attr("", "", "name", new);
            tree.set_attr("", "", "displayName", new);
        }
        let none = HashSet::new();
        let mut unreadable_formula = false;
        let mut bad_format = None;
        tree.walk_mut(&mut |n| {
            if n.ns != S {
                return;
            }
            if ["calculatedColumnFormula", "totalsRowFormula"].contains(&n.local.as_str()) {
                match maps.renames.rewrite(&n.text(), &none) {
                    Some(f) => {
                        let f = f.into_owned();
                        n.children = vec![xml::Child::Text(f)];
                    }
                    None => unreadable_formula = true,
                }
            }
            for a in &mut n.attrs {
                if a.ns.is_empty() && (a.local == "dxfId" || a.local.ends_with("DxfId")) {
                    match a
                        .value
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| maps.styles.dxfs.get(i))
                    {
                        Some(new) => a.value = new.to_string(),
                        None => bad_format = Some(a.value.clone()),
                    }
                }
            }
            if n.local == "tableStyleInfo"
                && let Some(new) = n
                    .attr("", "name")
                    .and_then(|s| maps.styles.table_styles.get(s))
            {
                let new = new.clone();
                n.set_attr("", "", "name", &new);
            }
        });
        if unreadable_formula {
            return Err(unreadable(origin, &format!("table {name}")));
        }
        if let Some(id) = bad_format {
            return Err(origin.bad(&format!(
                "table {name} uses format {id}, which isn't defined"
            )));
        }
        let mut out = declaration(&text);
        tree.write(&mut out);
        Ok(out.into_bytes())
    }

    fn finish(self, plan: &mut Plan, base: &Package) -> Result<(), Error> {
        let Self {
            book,
            mut rels,
            strings,
            styles,
            base_styles,
            ..
        } = self;
        strings.finish(plan, &mut rels, &book.part, &base.origin)?;
        if let Some(sheet) = styles.finish() {
            let head = base_styles.as_ref().map(|h| declaration(&h.text));
            let mut text = head.unwrap_or(DECLARATION.to_string());
            sheet.write(&mut text);
            match base_styles {
                Some(h) => plan.replace(&h.part, Body::Data(text.into_bytes())),
                None => {
                    let part = plan.claim("xl/styles.xml");
                    plan.type_part(&part, &format!("{SML}styles+xml"), false);
                    rels.add(&format!("{REL}styles"), relative(&book.part, &part), false);
                    plan.add(part, Body::Data(text.into_bytes()), None);
                }
            }
        }
        let mut text = declaration(&book.text);
        book.tree.write(&mut text);
        plan.replace(&book.part, Body::Data(text.into_bytes()));
        rels_part(plan, base, &book.part, &rels);
        Ok(())
    }
}

/// What the names of one appended workbook turned into so far.
struct Found {
    renames: a1::Renames,
    /// Per sheet, the lower-case names of its sheet-scope names.
    locals: Vec<HashSet<String>>,
    list: Vec<Rename>,
    origin: Origin,
}

impl Found {
    fn note(&mut self, input: usize, kind: &'static str, from: &str, to: &str) {
        self.list.push(Rename {
            input,
            kind,
            from: from.into(),
            to: to.into(),
        });
    }

    fn rewrite<'f>(
        &self,
        formula: &'f str,
        local: &HashSet<String>,
        place: &str,
    ) -> Result<Cow<'f, str>, Error> {
        self.renames
            .rewrite(formula, local)
            .ok_or_else(|| unreadable(&self.origin, place))
    }
}

/// How one appended workbook's content changes on its way into the output.
struct Maps {
    /// Shared-string indices.
    strings: Vec<u32>,
    styles: styles::Map,
    /// Set when the appended workbook's theme or palette isn't the output's.
    colours: Option<styles::Colours>,
    views: usize,
    renames: a1::Renames,
    /// Per sheet, the lower-case names of its sheet-scope names.
    locals: Vec<HashSet<String>>,
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

/// `e`'s start tag as written, with the attribute `name` set to `value`.
fn with_attr(
    r: &Reader,
    e: &BytesStart,
    empty: bool,
    name: &str,
    value: &str,
) -> Result<String, Error> {
    let mut s = format!("<{}", e.name().as_ref());
    let mut set = false;
    for a in e.attributes() {
        let a = a.map_err(|e| r.malformed(&e.to_string()))?;
        let key: &str = a.key.as_ref();
        let mine = key == name;
        set |= mine;
        let kept = if mine { value } else { &a.value };
        s += &format!(" {key}=\"{}\"", kept.replace('"', "&quot;"));
    }
    if !set {
        s += &format!(" {name}=\"{value}\"");
    }
    s += if empty { "/>" } else { ">" };
    Ok(s)
}

/// A number attribute of `e`, when it has it.
fn number(r: &Reader, e: &BytesStart, name: &str) -> Result<Option<u32>, Error> {
    for a in e.attributes() {
        let a = a.map_err(|e| r.malformed(&e.to_string()))?;
        if a.key.as_ref() == name {
            return Ok(a.value.parse().ok());
        }
    }
    Ok(None)
}

/// One appended sheet on its way out.
struct SheetJob<'a> {
    maps: &'a Maps,
    index: usize,
    name: &'a str,
}

impl SheetJob<'_> {
    /// `text` with everything that points elsewhere pointing at the output's.
    fn write(
        &self,
        text: &str,
        part: &str,
        origin: &Origin,
        out: &mut dyn std::io::Write,
    ) -> Result<(), Error> {
        let maps = self.maps;
        let local = &maps.locals[self.index];
        let mut r = Reader::new(text, part, origin);
        let mut p = Patcher {
            text,
            copied: 0,
            out,
        };
        let mut cell = None;
        let mut string_cell = false;
        let bad_format = |n: u32, place: String| {
            origin.bad(&format!("{place} uses format {n}, which isn't defined"))
        };
        let place = |cell: &Option<BytesStart>| {
            let at = cell.as_ref().and_then(|c| c.try_get_attribute("r").ok()?);
            match at {
                Some(a) => format!("{}!{}", self.name, a.value),
                None => self.name.to_string(),
            }
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
                "c" if r.element_ns(e) == S => {
                    cell = Some(e.clone());
                    string_cell = false;
                    let mut style = 0;
                    for a in e.attributes() {
                        let a = a.map_err(|e| r.malformed(&e.to_string()))?;
                        match a.key.as_ref() {
                            "t" => string_cell = a.value == "s",
                            "s" => style = a.value.parse().unwrap_or(u32::MAX),
                            _ => {}
                        }
                    }
                    let Some(new) = maps.styles.xf(style) else {
                        return Err(bad_format(style, place(&cell)));
                    };
                    if new != style {
                        let tag = with_attr(&r, e, empty, "s", &new.to_string())?;
                        p.replace(start..end, &tag)?;
                    }
                }
                local @ ("row" | "col") if r.element_ns(e) == S => {
                    let name = if local == "row" { "s" } else { "style" };
                    if let Some(style) = number(&r, e, name)? {
                        let Some(new) = maps.styles.xf(style) else {
                            return Err(bad_format(style, self.name.to_string()));
                        };
                        if new != style {
                            let tag = with_attr(&r, e, empty, name, &new.to_string())?;
                            p.replace(start..end, &tag)?;
                        }
                    }
                }
                "cfRule" | "sortCondition" | "colorFilter" if r.element_ns(e) == S => {
                    if let Some(dxf) = number(&r, e, "dxfId")? {
                        let Some(new) = maps.styles.dxfs.get(dxf as usize) else {
                            return Err(bad_format(dxf, self.name.to_string()));
                        };
                        let tag = with_attr(&r, e, empty, "dxfId", &new.to_string())?;
                        p.replace(start..end, &tag)?;
                    }
                }
                "v" if string_cell && !empty && r.element_ns(e) == S => {
                    let (value, span) = r.element_text()?;
                    let new = value
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| maps.strings.get(n));
                    let Some(new) = new else {
                        let why = format!("{} uses a shared string it doesn't have", place(&cell));
                        return Err(origin.bad(&why));
                    };
                    p.replace(span, &new.to_string())?;
                }
                "f" | "formula" | "formula1" | "formula2" if !empty => {
                    let ns = r.element_ns(e);
                    if ns == S || (ns == XM && e.local_name().as_ref() == "f") {
                        let (f, span) = r.element_text()?;
                        match maps.renames.rewrite(&f, local) {
                            Some(Cow::Owned(new)) => p.replace(span, &xml::escape_text(&new))?,
                            Some(_) => {}
                            None => return Err(unreadable(origin, &place(&cell))),
                        }
                    }
                }
                "cfvo" | "hyperlink" if r.element_ns(e) == S => {
                    let mut attrs = r.attrs(e)?;
                    let cfvo = e.local_name().as_ref() == "cfvo";
                    let wanted = if cfvo { "val" } else { "location" };
                    let formula = !cfvo
                        || attrs
                            .iter()
                            .any(|a| a.local == "type" && a.value == "formula");
                    let found = attrs
                        .iter_mut()
                        .find(|a| a.local == wanted && a.ns.is_empty());
                    if let Some(a) = found.filter(|_| formula) {
                        let new = maps.renames.rewrite(&a.value, local);
                        match new {
                            Some(Cow::Owned(new)) => {
                                a.value = new;
                                p.replace(
                                    start..end,
                                    &start_tag(e.name().as_ref(), &attrs, empty),
                                )?;
                            }
                            Some(_) => {}
                            // A link to a place that isn't a formula just stays as it is.
                            None if !cfvo => {}
                            None => return Err(unreadable(origin, self.name)),
                        }
                    }
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
                local => {
                    let ns = r.element_ns(e);
                    let colour = ns == X14
                        || (ns == S
                            && ["tabColor", "color", "fgColor", "bgColor"].contains(&local));
                    if let Some(c) = maps.colours.as_ref().filter(|_| colour) {
                        let mut attrs = r.attrs(e)?;
                        if c.resolve(&mut attrs) {
                            let tag = start_tag(e.name().as_ref(), &attrs, empty);
                            p.replace(start..end, &tag)?;
                        }
                    }
                }
            }
        }
    }
}
