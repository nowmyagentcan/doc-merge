//! PowerPoint: every deck's slides after the base's, with their masters, layouts and notes.
//! Slides and what hangs off them are copied as stored; only the main part and its
//! relationships are rebuilt.

use std::collections::{HashMap, HashSet};

use crate::Rename;
use crate::error::{Error, Warnings, thousands};
use crate::package::{MAX_PARSED, Package, Rel, parse_rels, rels_name, resolve};
use crate::writer::{Body, Part, Plan, R, Rels, planned_text, relative, rels_part};
use crate::xml::{self, Attr, Child, Node, XMLNS, declaration, prefix_of};

pub const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const MAX_SLIDES: usize = 10_000;
/// The children of `p:presentation`, in schema order.
const ORDER: [&str; 15] = [
    "sldMasterIdLst",
    "notesMasterIdLst",
    "handoutMasterIdLst",
    "sldIdLst",
    "sldSz",
    "notesSz",
    "smartTags",
    "embeddedFontLst",
    "custShowLst",
    "photoAlbum",
    "custDataLst",
    "kinsoku",
    "defaultTextStyle",
    "modifyVerifier",
    "extLst",
];
const MASTER_IDS: (u32, u32) = (1 << 31, u32::MAX);
const SLIDE_IDS: (u32, u32) = (256, (1 << 31) - 1);

/// What a merge of decks gives besides the plan.
pub struct Merged {
    pub plan: Plan,
    /// Each input's slide count.
    pub slides: Vec<usize>,
    pub renames: Vec<Rename>,
}

pub fn merge(inputs: &mut [Package], warnings: &mut Warnings) -> Result<Merged, Error> {
    let decks = inputs
        .iter_mut()
        .map(Deck::read)
        .collect::<Result<Vec<_>, _>>()?;
    let total: usize = decks.iter().map(|d| d.slides.len()).sum();
    if total > MAX_SLIDES {
        return Err(Error::limit_exceeded(
            "The merged deck's slide count",
            &thousands(total as u64),
            &thousands(MAX_SLIDES as u64),
        ));
    }
    let mut plan = Plan::base(&inputs[0], warnings)?;
    let mut out = Output::new(&decks[0], &inputs[0]);
    for (i, deck) in decks.iter().enumerate().skip(1) {
        out.append(&mut plan, inputs, i, deck)?;
    }
    out.finish(&mut plan, &inputs[0]);
    let main = inputs[0].main.clone();
    check(&plan, inputs, &main, decks[0].id_defects())?;
    Ok(Merged {
        plan,
        slides: decks.iter().map(|d| d.slides.len()).collect(),
        renames: vec![],
    })
}

/// A slide as its deck lists it.
struct Slide {
    part: String,
    id: u32,
}

/// What the main part of a deck says.
struct Deck {
    declaration: String,
    root: Node,
    rels: Vec<Rel>,
    masters: Vec<String>,
    /// Of masters and layouts.
    master_ids: Vec<u32>,
    notes_master: Option<String>,
    slides: Vec<Slide>,
}

/// The entries of list `list` in a main part or master.
fn listed<'a>(root: &'a Node, list: &str) -> impl Iterator<Item = &'a Node> {
    root.child(P, list).into_iter().flat_map(Node::nodes)
}

/// The part relationship `id` of `source` points at.
fn target(pkg: &Package, source: &str, rels: &[Rel], id: &str) -> Result<String, Error> {
    let found = rels
        .iter()
        .find(|r| r.id == id && !r.external)
        .and_then(|r| resolve(source, &r.target))
        .and_then(|t| pkg.find(&t));
    match found {
        Some(e) => Ok(e.name.clone()),
        None => Err(pkg
            .origin
            .bad(&format!("{source} points to a missing part {id}"))),
    }
}

/// The last segment of a relationship type.
fn kind(r: &Rel) -> &str {
    r.kind.rsplit('/').next().unwrap_or_default()
}

impl Deck {
    fn read(pkg: &mut Package) -> Result<Self, Error> {
        let main = pkg.main.clone();
        let text = pkg.text(&main, MAX_PARSED)?;
        let root = xml::tree(&text, &main, &pkg.origin)?;
        let rels = pkg.rels_of(&main)?;
        let part =
            |pkg: &Package, n: &Node| target(pkg, &main, &rels, n.attr(R, "id").unwrap_or(""));
        let mut masters = vec![];
        let mut master_ids = ids(&root, "sldMasterIdLst");
        for n in listed(&root, "sldMasterIdLst") {
            let master = part(pkg, n)?;
            let text = pkg.text(&master, MAX_PARSED)?;
            let tree = xml::tree(&text, &master, &pkg.origin)?;
            master_ids.extend(ids(&tree, "sldLayoutIdLst"));
            masters.push(master);
        }
        let notes_master = listed(&root, "notesMasterIdLst").next();
        let notes_master = notes_master.map(|n| part(pkg, n)).transpose()?;
        let mut slides = vec![];
        for n in listed(&root, "sldIdLst") {
            let slide = part(pkg, n)?;
            let own = pkg.rels_of(&slide)?;
            let count = |k: &str| own.iter().filter(|r| kind(r) == k).count();
            if count("slideLayout") != 1 || count("notesSlide") > 1 {
                let why = format!("{slide} doesn't have one layout and at most one notes page");
                return Err(pkg.origin.bad(&why));
            }
            let id = n.attr("", "id").and_then(|v| v.parse().ok());
            let Some(id) = id else {
                return Err(pkg
                    .origin
                    .bad(&format!("{main} lists {slide} without an id")));
            };
            slides.push(Slide { part: slide, id });
        }
        Ok(Self {
            declaration: declaration(&text),
            root,
            rels,
            masters,
            master_ids,
            notes_master,
            slides,
        })
    }

    /// How many of its own ids break the rules; a base keeps its ids as they are.
    fn id_defects(&self) -> usize {
        let slides = self.slides.iter().map(|s| s.id);
        defects(self.master_ids.iter().copied(), MASTER_IDS) + defects(slides, SLIDE_IDS)
    }
}

/// The ids of the entries of list `list`.
fn ids(root: &Node, list: &str) -> Vec<u32> {
    listed(root, list)
        .filter_map(|n| n.attr("", "id")?.parse().ok())
        .collect()
}

/// Ids out of range or repeated.
fn defects(ids: impl Iterator<Item = u32>, (low, high): (u32, u32)) -> usize {
    let mut seen = HashSet::new();
    ids.filter(|id| !(low..=high).contains(id) || !seen.insert(*id))
        .count()
}

/// New ids above the highest in use, going round to the lowest free one at the top of the range.
struct Ids {
    used: HashSet<u32>,
    next: u64,
    range: (u32, u32),
}

impl Ids {
    fn new(used: impl IntoIterator<Item = u32>, range: (u32, u32)) -> Self {
        let used: HashSet<u32> = used.into_iter().collect();
        let top = used.iter().filter(|id| **id >= range.0).max();
        let next = top.map_or(u64::from(range.0), |id| u64::from(*id) + 1);
        Self { used, next, range }
    }

    fn take(&mut self) -> u32 {
        loop {
            if self.next > u64::from(self.range.1) {
                self.next = u64::from(self.range.0);
            }
            let id = self.next as u32;
            self.next += 1;
            if self.used.insert(id) {
                return id;
            }
        }
    }
}

/// `p:` or whatever the main part calls that namespace.
fn prefix(root: &Node) -> String {
    root.name[..root.name.len() - root.local.len()].to_string()
}

/// The prefix of the relationships namespace on `root`, declared there when missing.
fn rel_prefix(root: &mut Node) -> String {
    if let Some(p) = prefix_of(&root.attrs, R) {
        return format!("{p}:");
    }
    let mut name = String::from("r");
    while root
        .attrs
        .iter()
        .any(|a| a.is_declaration() && a.local == name)
    {
        name.push('r');
    }
    root.attrs
        .push(Attr::new(&format!("xmlns:{name}"), XMLNS, R));
    format!("{name}:")
}

/// A child list of the main part, added in its schema place when missing.
fn list<'a>(root: &'a mut Node, local: &str) -> &'a mut Node {
    let at = ORDER
        .iter()
        .position(|o| *o == local)
        .expect("a known list");
    root.child_or_insert(P, local, &ORDER[..at])
}

/// The merged main part and what it refers to.
struct Output {
    main: String,
    declaration: String,
    root: Node,
    rels: Rels,
    /// Masters and layouts share one pool.
    master_ids: Ids,
    slide_ids: Ids,
    notes_master: Option<String>,
}

impl Output {
    fn new(deck: &Deck, pkg: &Package) -> Self {
        Self {
            main: pkg.main.clone(),
            declaration: deck.declaration.clone(),
            root: deck.root.clone(),
            rels: Rels::new(deck.rels.clone()),
            master_ids: Ids::new(deck.master_ids.iter().copied(), MASTER_IDS),
            slide_ids: Ids::new(deck.slides.iter().map(|s| s.id), SLIDE_IDS),
            notes_master: deck.notes_master.clone(),
        }
    }

    /// Adds entry `local` to list `of` of the main part, with a new relationship to `part`.
    fn add_entry(&mut self, of: &str, local: &str, id: Option<u32>, kind: &str, part: &str) {
        let target = relative(&self.main, part);
        let rid = self.rels.add(&format!("{REL}{kind}"), target, false);
        let (p, r) = (prefix(&self.root), rel_prefix(&mut self.root));
        let mut attrs: Vec<Attr> = id
            .iter()
            .map(|id| Attr::new("id", "", &id.to_string()))
            .collect();
        attrs.push(Attr::new(&format!("{r}id"), R, &rid));
        let entry = Node::new(&format!("{p}{local}"), P, attrs);
        list(&mut self.root, of).children.push(Child::Node(entry));
    }

    /// Copies a master with its layouts, theme and pictures under new ids.
    fn add_master(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        master: &str,
    ) -> Result<(), Error> {
        let name = plan.copy_graph(inputs, i, master, &|_, _| None)?;
        let text = inputs[i].text(master, MAX_PARSED)?;
        let mut tree = xml::tree(&text, master, &inputs[i].origin)?;
        let id = self.master_ids.take();
        if let Some(layouts) = tree.child_mut(P, "sldLayoutIdLst") {
            for n in layouts.nodes_mut() {
                n.set_attr("", "", "id", &self.master_ids.take().to_string());
            }
        }
        plan.replace(&name, Body::Data(part_bytes(&declaration(&text), &tree)));
        self.add_entry(
            "sldMasterIdLst",
            "sldMasterId",
            Some(id),
            "slideMaster",
            &name,
        );
        Ok(())
    }

    fn append(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        deck: &Deck,
    ) -> Result<(), Error> {
        if let (None, Some(part)) = (&self.notes_master, &deck.notes_master) {
            let name = plan.copy_graph(inputs, i, part, &|_, _| None)?;
            self.add_entry(
                "notesMasterIdLst",
                "notesMasterId",
                None,
                "notesMaster",
                &name,
            );
            self.notes_master = Some(name);
        }
        for master in &deck.masters {
            self.add_master(plan, inputs, i, master)?;
        }
        let notes = self.notes_master.clone();
        let mapped = |r: &Rel, _: &str| notes.clone().filter(|_| kind(r) == "notesMaster");
        for slide in &deck.slides {
            let name = plan.copy_graph(inputs, i, &slide.part, &mapped)?;
            let id = self.slide_ids.take();
            self.add_entry("sldIdLst", "sldId", Some(id), "slide", &name);
        }
        Ok(())
    }

    fn finish(self, plan: &mut Plan, base: &Package) {
        plan.replace(
            &self.main,
            Body::Data(part_bytes(&self.declaration, &self.root)),
        );
        rels_part(plan, base, &self.main, &self.rels);
    }
}

/// A whole part as bytes, after the declaration of the text it came from.
fn part_bytes(declaration: &str, root: &Node) -> Vec<u8> {
    let mut s = declaration.to_string();
    root.write(&mut s);
    s.into_bytes()
}

/// The PowerPoint rules of a merged deck: slide, master and layout ids unique and in range
/// (but for the `allowed` defects the base came with), and each slide with one layout and at
/// most one notes page.
fn check(plan: &Plan, inputs: &mut [Package], main: &str, allowed: usize) -> Result<(), Error> {
    let parts: HashMap<String, &Part> = plan
        .parts
        .iter()
        .map(|p| (p.name.to_ascii_lowercase(), p))
        .collect();
    let origin = inputs[0].origin.clone();
    let mut text = |name: &str| -> Result<String, Error> {
        let part = parts.get(&name.to_ascii_lowercase());
        let text = part.map(|p| planned_text(p, inputs)).transpose()?.flatten();
        text.ok_or_else(|| Error::internal(&format!("part {name} is missing")))
    };
    let main = main.to_string();
    let rels_of = |text: &mut dyn FnMut(&str) -> Result<String, Error>, part: &str| {
        let name = rels_name(part);
        parse_rels(&text(&name)?, &name, &origin)
    };
    let root = xml::tree(&text(&main)?, &main, &origin)?;
    let rels = rels_of(&mut text, &main)?;
    let find = |source: &str, rels: &[Rel], n: &Node| {
        let id = n.attr(R, "id").unwrap_or("");
        let found = rels.iter().find(|r| r.id == id);
        found
            .and_then(|r| resolve(source, &r.target))
            .ok_or_else(|| Error::internal(&format!("{source} has no relationship {id}")))
    };
    let mut master_ids = ids(&root, "sldMasterIdLst");
    for n in listed(&root, "sldMasterIdLst") {
        let master = find(&main, &rels, n)?;
        master_ids.extend(ids(
            &xml::tree(&text(&master)?, &master, &origin)?,
            "sldLayoutIdLst",
        ));
    }
    let slide_ids = ids(&root, "sldIdLst");
    let found =
        defects(master_ids.into_iter(), MASTER_IDS) + defects(slide_ids.into_iter(), SLIDE_IDS);
    if found > allowed {
        return Err(Error::internal(&format!(
            "{found} slide or master ids break the rules"
        )));
    }
    for n in listed(&root, "sldIdLst") {
        let slide = find(&main, &rels, n)?;
        let own = rels_of(&mut text, &slide)?;
        let count = |k: &str| own.iter().filter(|r| kind(r) == k).count();
        if count("slideLayout") != 1 || count("notesSlide") > 1 {
            return Err(Error::internal(&format!(
                "{slide} has the wrong relationships"
            )));
        }
    }
    Ok(())
}
