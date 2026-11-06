//! PowerPoint: every deck's slides after the base's, with their masters, layouts and notes.
//! Slides and what hangs off them are copied as stored; the main part, its relationships and
//! the lists every deck shares (comment authors, table styles, embedded fonts) are rebuilt.

mod masters;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::Rename;
use crate::error::{Error, Warning, Warnings, thousands};
use crate::package::{Entry, MAX_PARSED, Package, Rel, parse_rels, rels_name, resolve, unescape};
use crate::writer::{Body, Part, Plan, R, Rels, guid, planned_text, relative, rels_part};
use crate::xml::{self, A, Attr, Child, Node, Prefixes, XMLNS, declaration, declared_prefix};
use masters::{Design, Fit, Units, ratio};

pub const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const P14: &str = "http://schemas.microsoft.com/office/powerpoint/2010/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const AUTHORS: &str = "http://schemas.microsoft.com/office/2018/10/relationships/authors";
const P188: &str = "http://schemas.microsoft.com/office/powerpoint/2018/8/main";
const COMMANDS: &str = "http://schemas.microsoft.com/office/powerpoint/2013/main/command";
const PROPERTIES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/extended-properties";
const DATA_STORE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/customXml";
const ITEM_PROPERTIES: &str =
    "application/vnd.openxmlformats-officedocument.customXmlProperties+xml";
const SECTIONS: &str = "{521415D9-36F7-43E2-AB2F-B90AF26B5E84}";
/// Relationships of a main part this module handles, or that only the base's copy keeps. The
/// last two are bookkeeping of shared editing.
const KNOWN_PARTS: [&str; 14] = [
    "slideMaster",
    "slide",
    "notesMaster",
    "handoutMaster",
    "presProps",
    "viewProps",
    "theme",
    "tableStyles",
    "commentAuthors",
    "authors",
    "font",
    "customXml",
    "revisionInfo",
    "changesInfo",
];
const SHOW_LINK: &str = "ppaction://customshow?id=";
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
const SHOW_IDS: (u32, u32) = (0, u32::MAX);
/// A deck that doesn't say its slide size has this one.
const DEFAULT_SIZE: (u64, u64) = (9_144_000, 6_858_000);

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
    for (pkg, deck) in inputs.iter().zip(&decks).skip(1) {
        if deck.slides.iter().any(|s| s.zoom) {
            let name = &pkg.origin.name;
            let e = Error::unsupported_variant(name, "slide or section zoom");
            return Err(e.at(pkg.origin.input));
        }
    }
    let total: usize = decks.iter().map(|d| d.slides.len()).sum();
    if total > MAX_SLIDES {
        return Err(Error::limit_exceeded(
            "The merged deck's slide count",
            &thousands(total as u64),
            &thousands(MAX_SLIDES as u64),
        ));
    }
    let mut plan = Plan::base(&inputs[0], warnings)?;
    let mut renames = vec![];
    let sectioned = decks.iter().any(|d| !d.sections.is_empty());
    let mut out = Output::new(&decks[0], &mut inputs[0], sectioned)?;
    for master in &decks[0].masters {
        let design = Design::of(&mut inputs[0], master)?;
        let (part, layouts) = (master.part.clone(), master.layouts.clone());
        out.units.add(decks[0].size, design, 0, part, layouts);
    }
    for (i, deck) in decks.iter().enumerate().skip(1) {
        out.append(&mut plan, inputs, i, deck, &mut renames)?;
        leave_warnings(&inputs[i], deck, warnings);
        if deck.size != decks[0].size && !deck.slides.is_empty() {
            let (from, to) = (ratio(deck.size), ratio(decks[0].size));
            let origin = &inputs[i].origin;
            warnings.push(Warning::slides_scaled(i + 1, &origin.name, &from, &to));
        }
    }
    out.finish(&mut plan, &mut inputs[0])?;
    check(&plan, inputs, decks[0].id_defects())?;
    Ok(Merged {
        plan,
        slides: decks.iter().map(|d| d.slides.len()).collect(),
        renames,
    })
}

/// A slide as its deck lists it.
struct Slide {
    part: String,
    /// Its relationship in the main part.
    rid: String,
    id: u32,
    /// `p14:creationId`.
    creation: Option<String>,
    /// Whether a link on it runs a custom show.
    runs_show: bool,
    hidden: bool,
    notes: bool,
    /// A slide or section zoom, which points at slides and sections by id.
    zoom: bool,
    /// Its comment parts, and whether each is a modern one.
    comments: Vec<(String, bool)>,
}

/// A small list part of a main part, held whole.
#[derive(Clone)]
struct Held {
    part: String,
    /// Its relationship's type.
    kind: String,
    content_type: String,
    declaration: String,
    root: Node,
    changed: bool,
}

impl Held {
    fn read(pkg: &mut Package, rels: &[Rel], kind: &str) -> Result<Option<Self>, Error> {
        let Some(r) = rels.iter().find(|r| r.kind == kind && !r.external) else {
            return Ok(None);
        };
        let main = pkg.main.clone();
        let part = target(pkg, &main, rels, &r.id)?;
        let text = pkg.text(&part, MAX_PARSED)?;
        Ok(Some(Self {
            content_type: pkg.types.of(&part).unwrap_or_default().to_string(),
            kind: kind.to_string(),
            declaration: declaration(&text),
            root: xml::tree(&text, &part, &pkg.origin)?,
            part,
            changed: false,
        }))
    }

    /// Adds an element of a part whose root has `declared` on it.
    fn add(&mut self, mut node: Node, declared: &[Attr]) {
        let mut prefixes = Prefixes::of(&self.root.attrs);
        node.attrs.extend(prefixes.merge(declared));
        self.root.attrs = prefixes.attrs;
        self.root.children.push(Child::Node(node));
        self.changed = true;
    }
}

/// A slide master and its layouts, in their order.
struct Master {
    part: String,
    layouts: Vec<String>,
}

/// What the main part of a deck says.
struct Deck {
    declaration: String,
    root: Node,
    rels: Vec<Rel>,
    size: (u64, u64),
    masters: Vec<Master>,
    /// Of masters and layouts.
    master_ids: Vec<u32>,
    notes_master: Option<String>,
    slides: Vec<Slide>,
    /// `p14:section` elements.
    sections: Vec<Node>,
    /// The file name without its extension.
    stem: String,
    comment_authors: Option<Held>,
    authors: Option<Held>,
    table_styles: Option<Held>,
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
            let own = pkg.rels_of(&master)?;
            master_ids.extend(ids(&tree, "sldLayoutIdLst"));
            let layouts = listed(&tree, "sldLayoutIdLst")
                .map(|n| target(pkg, &master, &own, n.attr(R, "id").unwrap_or("")))
                .collect::<Result<_, _>>()?;
            masters.push(Master {
                part: master,
                layouts,
            });
        }
        let related = rels.iter().find(|r| kind(r) == "notesMaster");
        let notes_master = match listed(&root, "notesMasterIdLst").next() {
            Some(n) => Some(part(pkg, n)?),
            None => related
                .map(|r| target(pkg, &main, &rels, &r.id))
                .transpose()?,
        };
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
            let text = pkg.text(&slide, MAX_PARSED)?;
            let tree = xml::tree(&text, &slide, &pkg.origin)?;
            let (mut creation, mut zoom) = (None, false);
            tree.walk(&mut |n| {
                if n.is(P14, "creationId") {
                    creation = n.attr("", "val").map(String::from);
                }
                zoom |= n.ns.contains("/powerpoint/") && n.ns.ends_with("zoom");
            });
            let comments = own
                .iter()
                .filter(|r| kind(r) == "comments")
                .map(|r| {
                    let part = target(pkg, &slide, &own, &r.id)?;
                    Ok((part, !r.kind.starts_with(REL)))
                })
                .collect::<Result<_, Error>>()?;
            slides.push(Slide {
                part: slide,
                rid: n.attr(R, "id").unwrap_or("").to_string(),
                id,
                creation,
                runs_show: text.contains(SHOW_LINK),
                hidden: tree.attr("", "show") == Some("0"),
                notes: count("notesSlide") == 1,
                zoom,
                comments,
            });
        }
        let size = slide_size(&root).ok_or_else(|| {
            pkg.origin.bad(&format!(
                "{main} has a slide size that isn't a positive number"
            ))
        })?;
        let sections = section_list(&root).map_or(vec![], |l| l.nodes().cloned().collect());
        let stem = Path::new(&*pkg.origin.name).file_stem();
        let stem = stem.unwrap_or_default().to_string_lossy().into_owned();
        Ok(Self {
            declaration: declaration(&text),
            comment_authors: Held::read(pkg, &rels, &format!("{REL}commentAuthors"))?,
            authors: Held::read(pkg, &rels, AUTHORS)?,
            table_styles: Held::read(pkg, &rels, &format!("{REL}tableStyles"))?,
            sections,
            stem,
            size,
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

/// Says what of an appended deck stays behind: its protection, and document-level parts this
/// merge knows nothing about.
fn leave_warnings(pkg: &Package, deck: &Deck, warnings: &mut Warnings) {
    let (name, input) = (&pkg.origin.name, pkg.origin.input);
    if deck.root.child(P, "modifyVerifier").is_some() {
        warnings.push(Warning::protection_not_kept(input, name));
    }
    let mut dropped: Vec<(&str, usize)> = vec![];
    for r in &deck.rels {
        let kind = kind(r);
        if KNOWN_PARTS.contains(&kind) || r.external {
            continue;
        }
        match dropped.iter_mut().find(|d| d.0 == kind) {
            Some(d) => d.1 += 1,
            None => dropped.push((kind, 1)),
        }
    }
    dropped.sort();
    if !dropped.is_empty() {
        warnings.push(Warning::part_dropped(input, name, &dropped));
    }
}

/// The list part `slot` of the output, created like `from` when the base has none.
fn output_list<'a>(
    slot: &'a mut Option<Held>,
    from: &Held,
    plan: &mut Plan,
    rels: &mut Rels,
    main: &str,
) -> &'a mut Held {
    slot.get_or_insert_with(|| {
        let name = plan.claim(&from.part);
        plan.type_part(&name, &from.content_type, false);
        plan.add(name.clone(), Body::Data(vec![]), None);
        rels.add(&from.kind, relative(main, &name), false);
        let mut list = from.clone();
        list.root.children.clear();
        list.part = name;
        list.changed = true;
        list
    })
}

/// What the ids of a legacy comment become: per old author id, the new one and what to add to
/// each comment number.
type CommentMap = HashMap<String, (String, u64)>;

fn number(n: &Node, name: &str) -> u64 {
    n.attr("", name).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Takes a deck's legacy comment authors into the output's list: the same name and initials
/// are one author, whose comment numbers carry on from the highest so far.
fn merge_comment_authors(list: &mut Held, deck: &Held) -> CommentMap {
    let mut map = CommentMap::new();
    for author in deck.root.nodes().filter(|n| n.is(P, "cmAuthor")) {
        let old = author.attr("", "id").unwrap_or_default().to_string();
        let last = number(author, "lastIdx");
        let same = |n: &&mut Node| {
            n.is(P, "cmAuthor")
                && n.attr("", "name") == author.attr("", "name")
                && n.attr("", "initials") == author.attr("", "initials")
        };
        if let Some(n) = list.root.nodes_mut().find(same) {
            let offset = number(n, "lastIdx");
            n.set_attr("", "", "lastIdx", &(offset + last).to_string());
            let id = n.attr("", "id").unwrap_or_default().to_string();
            map.insert(old, (id, offset));
            list.changed = true;
            continue;
        }
        let top = list.root.nodes().map(|n| number(n, "id") + 1).max();
        let id = top.unwrap_or(0).to_string();
        let mut new = author.clone();
        new.set_attr("", "", "id", &id);
        list.add(new, &deck.root.attrs);
        map.insert(old, (id, 0));
    }
    map
}

/// Renumbers the comments of a legacy comment part, and the replies in them.
fn renumber_comments(root: &mut Node, map: &CommentMap) {
    root.walk_mut(&mut |n| {
        if n.local != "cm" && n.local != "parentCm" {
            return;
        }
        let author = n.attr("", "authorId").unwrap_or_default();
        if let Some((id, offset)) = map.get(author).cloned() {
            let idx = number(n, "idx") + offset;
            n.set_attr("", "", "authorId", &id);
            n.set_attr("", "", "idx", &idx.to_string());
        }
    });
}

/// Adds the elements of list `deck` the output's list lacks, compared by attribute `key`.
fn merge_by_id(list: &mut Held, deck: &Held, ns: &str, local: &str, key: &str) {
    let id = |n: &Node| n.attr("", key).map(str::to_ascii_uppercase);
    let mut known: HashSet<_> = list.root.nodes().filter_map(id).collect();
    for n in deck.root.nodes().filter(|n| n.is(ns, local)) {
        if id(n).is_some_and(|i| known.insert(i)) {
            list.add(n.clone(), &deck.root.attrs);
        }
    }
}

/// The `p14:sectionLst` of a main part.
fn section_list(root: &Node) -> Option<&Node> {
    root.child(P, "extLst")?
        .nodes()
        .find(|n| n.attr("", "uri") == Some(SECTIONS))?
        .child(P14, "sectionLst")
}

/// Points every link that runs a custom show at the show's new id.
fn relink_shows(root: &mut Node, shows: &HashMap<u32, u32>) {
    root.walk_mut(&mut |n| {
        for a in n.attrs.iter_mut().filter(|a| a.local == "action") {
            let Some(rest) = a.value.strip_prefix(SHOW_LINK) else {
                continue;
            };
            let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            if let Some(new) = rest[..digits].parse().ok().and_then(|id| shows.get(&id)) {
                a.value = format!("{SHOW_LINK}{new}{}", &rest[digits..]);
            }
        }
    });
}

/// A deck's slide size; none when it can't be read.
fn slide_size(root: &Node) -> Option<(u64, u64)> {
    let Some(n) = root.child(P, "sldSz") else {
        return Some(DEFAULT_SIZE);
    };
    let side = |name: &str| n.attr("", name)?.parse().ok().filter(|v| *v > 0);
    Some((side("cx")?, side("cy")?))
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
    units: Units,
    size: (u64, u64),
    show_ids: Ids,
    /// Lower-case custom show names.
    show_names: HashSet<String>,
    creations: HashSet<String>,
    /// The section list being built, if any deck has one.
    sections: Option<Vec<Node>>,
    /// The prefix of `p14:` elements, colon included.
    p14: String,
    section_ids: HashSet<String>,
    comment_authors: Option<Held>,
    authors: Option<Held>,
    table_styles: Option<Held>,
    /// Upper-case ids of modern comments.
    comment_ids: HashSet<String>,
    /// Upper-case ids of custom XML data stores.
    item_ids: HashSet<String>,
    /// Lower-case names of embedded fonts.
    fonts: HashSet<String>,
    notes_added: u64,
    hidden_added: u64,
}

impl Output {
    fn new(deck: &Deck, pkg: &mut Package, sectioned: bool) -> Result<Self, Error> {
        let mut comment_ids = HashSet::new();
        let modern = deck.slides.iter().flat_map(|s| &s.comments);
        for (part, _) in modern.filter(|c| c.1) {
            let text = pkg.text(part, MAX_PARSED)?;
            let tree = xml::tree(&text, part, &pkg.origin)?;
            tree.walk(&mut |n| {
                if n.ns == P188 && (n.local == "cm" || n.local == "reply") {
                    comment_ids.extend(n.attr("", "id").map(str::to_ascii_uppercase));
                }
            });
        }
        let mut item_ids = HashSet::new();
        for part in item_parts(pkg) {
            let text = pkg.text(&part, MAX_PARSED)?;
            let tree = xml::tree(&text, &part, &pkg.origin)?;
            item_ids.extend(tree.attr(DATA_STORE, "itemID").map(str::to_ascii_uppercase));
        }
        let fonts = listed(&deck.root, "embeddedFontLst").filter_map(font_name);
        let fonts = fonts.collect();
        let shows = listed(&deck.root, "custShowLst");
        let names = shows.filter_map(|n| n.attr("", "name"));
        let mut out = Self {
            main: pkg.main.clone(),
            declaration: deck.declaration.clone(),
            root: deck.root.clone(),
            rels: Rels::new(deck.rels.clone()),
            master_ids: Ids::new(deck.master_ids.iter().copied(), MASTER_IDS),
            slide_ids: Ids::new(deck.slides.iter().map(|s| s.id), SLIDE_IDS),
            notes_master: deck.notes_master.clone(),
            units: Units::default(),
            size: deck.size,
            show_ids: Ids::new(ids(&deck.root, "custShowLst"), SHOW_IDS),
            show_names: names.map(str::to_lowercase).collect(),
            creations: deck
                .slides
                .iter()
                .filter_map(|s| s.creation.clone())
                .collect(),
            sections: sectioned.then(Vec::new),
            p14: section_list(&deck.root).map_or("p14:".into(), prefix),
            section_ids: HashSet::new(),
            comment_authors: deck.comment_authors.clone(),
            authors: deck.authors.clone(),
            table_styles: deck.table_styles.clone(),
            comment_ids,
            item_ids,
            fonts,
            notes_added: 0,
            hidden_added: 0,
        };
        let same = deck.slides.iter().map(|s| (s.id, s.id)).collect();
        out.add_sections(deck, 0, &same);
        if listed(&deck.root, "notesMasterIdLst").next().is_none()
            && let Some(r) = deck.rels.iter().find(|r| kind(r) == "notesMaster")
        {
            out.list_entry("notesMasterIdLst", "notesMasterId", None, &r.id);
        }
        Ok(out)
    }

    /// Takes in a deck's authors and table styles; returns how its legacy comments renumber.
    fn merge_lists(&mut self, plan: &mut Plan, deck: &Deck) -> CommentMap {
        let (rels, main) = (&mut self.rels, self.main.as_str());
        if let Some(from) = &deck.authors {
            let list = output_list(&mut self.authors, from, plan, rels, main);
            merge_by_id(list, from, P188, "author", "id");
        }
        if let Some(from) = &deck.table_styles {
            let list = output_list(&mut self.table_styles, from, plan, rels, main);
            merge_by_id(list, from, A, "tblStyle", "styleId");
        }
        deck.comment_authors
            .as_ref()
            .map_or(CommentMap::new(), |from| {
                let list = output_list(&mut self.comment_authors, from, plan, rels, main);
                merge_comment_authors(list, from)
            })
    }

    /// Adds the embedded fonts of a deck the output lacks, with their font parts.
    fn add_fonts(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        deck: &Deck,
        local: &[Attr],
    ) -> Result<(), Error> {
        for font in listed(&deck.root, "embeddedFontLst") {
            if !font_name(font).is_some_and(|name| self.fonts.insert(name)) {
                continue;
            }
            let mut font = font.clone();
            font.attrs.extend(local.iter().cloned());
            for face in font.nodes_mut() {
                let Some(a) = face.attrs.iter_mut().find(|a| a.ns == R) else {
                    continue;
                };
                let part = target(&inputs[i], &inputs[i].main, &deck.rels, &a.value)?;
                let name = plan.copy_graph(inputs, i, &part, &|_, _| None)?;
                let to = relative(&self.main, &name);
                a.value = self.rels.add(&format!("{REL}font"), to, false);
            }
            self.root.set_attr("", "", "embedTrueTypeFonts", "1");
            list(&mut self.root, "embeddedFontLst")
                .children
                .push(Child::Node(font));
        }
        Ok(())
    }

    /// Fits a modern comment part to its copied slide: the slide's ids, and comment ids the
    /// output has already.
    fn renumber_modern(&mut self, root: &mut Node, slide: u32, creation: Option<&str>, i: usize) {
        root.walk_mut(&mut |n| {
            if n.ns == COMMANDS && n.local == "sldMk" {
                n.set_attr("", "", "sldId", &slide.to_string());
                if let Some(id) = creation {
                    n.set_attr("", "", "cId", id);
                }
            }
            if n.ns == P188 && (n.local == "cm" || n.local == "reply") {
                let old = n.attr("", "id").unwrap_or_default().to_string();
                if !self.comment_ids.insert(old.to_ascii_uppercase()) {
                    let id = free_guid(&mut self.comment_ids, i, &old);
                    n.set_attr("", "", "id", &id);
                }
            }
        });
    }

    /// Gives custom XML data stores copied from input `i` new ids where the output has theirs.
    fn renumber_items(
        &mut self,
        plan: &mut Plan,
        pkg: &mut Package,
        i: usize,
    ) -> Result<(), Error> {
        for part in item_parts(pkg) {
            if plan.copied(i, &part).is_none() {
                continue;
            }
            let text = pkg.text(&part, MAX_PARSED)?;
            let tree = xml::tree(&text, &part, &pkg.origin)?;
            let old = tree
                .attr(DATA_STORE, "itemID")
                .unwrap_or_default()
                .to_string();
            if self.item_ids.insert(old.to_ascii_uppercase()) {
                continue;
            }
            let id = free_guid(&mut self.item_ids, i, &old);
            edit_part(plan, pkg, i, &part, |root| {
                root.set_attr(DATA_STORE, "ds", "itemID", &id);
            })?;
        }
        Ok(())
    }

    /// Adds a deck's sections with the slides' new ids; a deck without sections gets one named
    /// after its file. A section id already taken gets a new one.
    fn add_sections(&mut self, deck: &Deck, input: usize, slide_ids: &HashMap<u32, u32>) {
        if self.sections.is_none() {
            return;
        }
        let p = self.p14.clone();
        let mut found = vec![];
        for section in &deck.sections {
            let mut section = section.clone();
            section.spell(P14, &p);
            let old = section.attr("", "id").unwrap_or_default().to_string();
            let id = self.section_id(&old, input);
            section.set_attr("", "", "id", &id);
            if let Some(list) = section.child_mut(P14, "sldIdLst") {
                list.children.retain_mut(|c| {
                    let Child::Node(n) = c else { return true };
                    let old = n.attr("", "id").and_then(|v| v.parse().ok());
                    let new = old.and_then(|id: u32| slide_ids.get(&id));
                    new.map(|id| n.set_attr("", "", "id", &id.to_string()))
                        .is_some()
                });
            }
            found.push(section);
        }
        if deck.sections.is_empty() {
            let id = self.section_id("", input);
            let attrs = vec![Attr::new("name", "", &deck.stem), Attr::new("id", "", &id)];
            let mut section = Node::new(&format!("{p}section"), P14, attrs);
            let mut list = Node::new(&format!("{p}sldIdLst"), P14, vec![]);
            for slide in &deck.slides {
                let id = Attr::new("id", "", &slide_ids[&slide.id].to_string());
                let entry = Node::new(&format!("{p}sldId"), P14, vec![id]);
                list.children.push(Child::Node(entry));
            }
            section.children.push(Child::Node(list));
            found.push(section);
        }
        self.sections.as_mut().expect("checked").extend(found);
    }

    /// `old`, unless the output has that section id already.
    fn section_id(&mut self, old: &str, input: usize) -> String {
        if !old.is_empty() && self.section_ids.insert(old.to_ascii_uppercase()) {
            return old.to_string();
        }
        free_guid(&mut self.section_ids, input, old)
    }

    /// The id a slide keeps or gets for `p14:creationId`, when it must change.
    fn new_creation(&mut self, old: &str, input: usize) -> Option<String> {
        if self.creations.insert(old.to_string()) {
            return None;
        }
        let fresh = (0u32..).map(|attempt| {
            let mut h = DefaultHasher::new();
            (input, old, attempt).hash(&mut h);
            (h.finish() as u32).max(1).to_string()
        });
        let id = { fresh }.find(|id| !self.creations.contains(id));
        self.creations.extend(id.clone());
        id
    }

    /// Gives a deck's custom shows new ids and free names; returns the old and new ids.
    fn plan_shows(
        &mut self,
        deck: &Deck,
        input: usize,
        renames: &mut Vec<Rename>,
    ) -> Vec<(u32, u32, String)> {
        let mut shows = vec![];
        for n in listed(&deck.root, "custShowLst") {
            let name = n.attr("", "name").unwrap_or_default();
            let free = std::iter::once(name.to_string())
                .chain((input + 1..).map(|k| format!("{name} ({k})")))
                .find(|n| self.show_names.insert(n.to_lowercase()))
                .expect("a free name");
            if free != name {
                renames.push(Rename {
                    input: input + 1,
                    kind: "custom_show",
                    from: name.into(),
                    to: free.clone(),
                });
            }
            let old = n.attr("", "id").and_then(|v| v.parse().ok()).unwrap_or(0);
            shows.push((old, self.show_ids.take(), free));
        }
        shows
    }

    /// Appends a deck's custom shows, now that its slides have their relationships.
    fn add_shows(
        &mut self,
        deck: &Deck,
        shows: &[(u32, u32, String)],
        rids: &HashMap<&str, String>,
        local: &[Attr],
    ) {
        for (n, (_, id, name)) in listed(&deck.root, "custShowLst").zip(shows) {
            let mut show = n.clone();
            show.set_attr("", "", "name", name);
            show.set_attr("", "", "id", &id.to_string());
            show.attrs.extend(local.iter().cloned());
            if let Some(slides) = show.child_mut(P, "sldLst") {
                slides.children.retain_mut(|c| match c {
                    Child::Node(n) => {
                        let id = n.attrs.iter_mut().find(|a| a.ns == R && a.local == "id");
                        match id.map(|a| (rids.get(a.value.as_str()), a)) {
                            Some((Some(new), a)) => {
                                a.value = new.clone();
                                true
                            }
                            _ => false,
                        }
                    }
                    Child::Text(_) => true,
                });
            }
            list(&mut self.root, "custShowLst")
                .children
                .push(Child::Node(show));
        }
    }

    /// Adds entry `local` to list `of` of the main part, with a new relationship to `part`;
    /// returns the relationship's id.
    fn add_entry(
        &mut self,
        of: &str,
        local: &str,
        id: Option<u32>,
        kind: &str,
        part: &str,
    ) -> String {
        let target = relative(&self.main, part);
        let rid = self.rels.add(&format!("{REL}{kind}"), target, false);
        self.list_entry(of, local, id, &rid);
        rid
    }

    /// Adds an entry for the relationship `rid` to the list `of` of the main part.
    fn list_entry(&mut self, of: &str, local: &str, id: Option<u32>, rid: &str) {
        let (p, r) = (prefix(&self.root), declared_prefix(&mut self.root, R, "r"));
        let mut attrs: Vec<Attr> = id
            .iter()
            .map(|id| Attr::new("id", "", &id.to_string()))
            .collect();
        attrs.push(Attr::new(&format!("{r}:id"), R, rid));
        let entry = Node::new(&format!("{p}{local}"), P, attrs);
        list(&mut self.root, of).children.push(Child::Node(entry));
    }

    /// Copies a master with its layouts, theme and pictures under new ids, fitted to the
    /// output's slide size; returns its name.
    fn add_master(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        master: &Master,
        fit: Option<&Fit>,
        shows: &HashMap<u32, u32>,
    ) -> Result<String, Error> {
        let notes = self.notes_master.clone();
        let mapped = |r: &Rel, _: &str| notes.clone().filter(|_| kind(r) == "notesMaster");
        let name = plan.copy_graph(inputs, i, &master.part, &mapped)?;
        let text = inputs[i].text(&master.part, MAX_PARSED)?;
        let mut tree = xml::tree(&text, &master.part, &inputs[i].origin)?;
        let id = self.master_ids.take();
        if let Some(layouts) = tree.child_mut(P, "sldLayoutIdLst") {
            for n in layouts.nodes_mut() {
                n.set_attr("", "", "id", &self.master_ids.take().to_string());
            }
        }
        relink_shows(&mut tree, shows);
        if let Some(fit) = fit {
            fit.apply(&mut tree);
        }
        for layout in &master.layouts {
            let text = inputs[i].text(layout, MAX_PARSED)?;
            if fit.is_some() || text.contains(SHOW_LINK) {
                edit_part(plan, &mut inputs[i], i, layout, |root| {
                    relink_shows(root, shows);
                    fit.iter().for_each(|f| f.apply(root));
                })?;
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
        Ok(name)
    }

    /// Shares each of a deck's masters that the output already holds and copies the others;
    /// returns where the shared parts of the deck went.
    fn add_masters(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        deck: &Deck,
        shows: &HashMap<u32, u32>,
    ) -> Result<HashMap<String, String>, Error> {
        let mut shared = HashMap::new();
        let fit = Fit::new(deck.size, self.size);
        for master in &deck.masters {
            let design = Design::of(&mut inputs[i], master)?;
            if let Some(unit) = self.units.find(deck.size, &design, i, inputs)? {
                shared.insert(master.part.to_ascii_lowercase(), unit.master.clone());
                for (ours, theirs) in master.layouts.iter().zip(&unit.layouts) {
                    shared.insert(ours.to_ascii_lowercase(), theirs.clone());
                }
                continue;
            }
            let name = self.add_master(plan, inputs, i, master, fit.as_ref(), shows)?;
            let copied = master.layouts.iter().filter_map(|l| plan.copied(i, l));
            let layouts = copied.map(String::from).collect();
            self.units.add(deck.size, design, i, name, layouts);
        }
        Ok(shared)
    }

    fn append(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        deck: &Deck,
        renames: &mut Vec<Rename>,
    ) -> Result<(), Error> {
        let mut prefixes = Prefixes::of(&self.root.attrs);
        let local = prefixes.merge(&deck.root.attrs);
        self.root.attrs = prefixes.attrs;
        let planned = self.plan_shows(deck, i, renames);
        let shows: HashMap<u32, u32> = planned.iter().map(|s| (s.0, s.1)).collect();
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
        let comment_map = self.merge_lists(plan, deck);
        self.add_fonts(plan, inputs, i, deck, &local)?;
        let shared = self.add_masters(plan, inputs, i, deck, &shows)?;
        let notes = self.notes_master.clone();
        let mapped = |r: &Rel, target: &str| match kind(r) {
            "notesMaster" => notes.clone(),
            "slideLayout" | "slideMaster" => shared.get(&target.to_ascii_lowercase()).cloned(),
            _ => None,
        };
        let fit = Fit::new(deck.size, self.size);
        let mut slide_ids = HashMap::new();
        let mut rids = HashMap::new();
        for slide in &deck.slides {
            let name = plan.copy_graph(inputs, i, &slide.part, &mapped)?;
            let old_creation = slide.creation.as_ref();
            let creation = old_creation.and_then(|old| self.new_creation(old, i));
            if fit.is_some() || creation.is_some() || slide.runs_show {
                edit_part(plan, &mut inputs[i], i, &slide.part, |root| {
                    relink_shows(root, &shows);
                    fit.iter().for_each(|f| f.apply(root));
                    root.walk_mut(&mut |n| {
                        if let (true, Some(id)) = (n.is(P14, "creationId"), &creation) {
                            n.set_attr("", "", "val", id);
                        }
                    });
                })?;
            }
            let id = self.slide_ids.take();
            let rid = self.add_entry("sldIdLst", "sldId", Some(id), "slide", &name);
            slide_ids.insert(slide.id, id);
            rids.insert(slide.rid.as_str(), rid);
            self.notes_added += u64::from(slide.notes);
            self.hidden_added += u64::from(slide.hidden);
            for (part, modern) in &slide.comments {
                edit_part(plan, &mut inputs[i], i, part, |root| {
                    if *modern {
                        self.renumber_modern(root, id, creation.as_deref(), i);
                    } else {
                        renumber_comments(root, &comment_map);
                    }
                })?;
            }
        }
        self.renumber_items(plan, &mut inputs[i], i)?;
        self.add_shows(deck, &planned, &rids, &local);
        self.add_sections(deck, i, &slide_ids);
        Ok(())
    }

    /// Writes the section list into the main part's extensions.
    fn write_sections(&mut self) {
        let Some(sections) = self.sections.take() else {
            return;
        };
        let p = prefix(&self.root);
        let sections: Vec<Child> = sections.into_iter().map(Child::Node).collect();
        let extensions = list(&mut self.root, "extLst");
        let found = extensions
            .nodes_mut()
            .find(|n| n.attr("", "uri") == Some(SECTIONS))
            .and_then(|n| n.child_mut(P14, "sectionLst"));
        if let Some(list) = found {
            list.children = sections;
            return;
        }
        let name = self.p14.clone();
        let declared = Attr::new(&format!("xmlns:{}", name.trim_end_matches(':')), XMLNS, P14);
        let mut list = Node::new(&format!("{name}sectionLst"), P14, vec![declared]);
        list.children = sections;
        let attrs = vec![Attr::new("uri", "", SECTIONS)];
        let mut extension = Node::new(&format!("{p}ext"), P, attrs);
        extension.children.push(Child::Node(list));
        extensions.children.insert(0, Child::Node(extension));
    }

    /// Updates the base's counts of slides, notes and hidden slides, where it has them.
    fn write_counts(&self, plan: &mut Plan, pkg: &mut Package) -> Result<(), Error> {
        let properties = pkg.rels.iter().find(|r| kind(r) == "extended-properties");
        let part = properties.and_then(|r| resolve("", &r.target));
        let Some(part) = part.filter(|p| pkg.has(p)) else {
            return Ok(());
        };
        let text = pkg.text(&part, MAX_PARSED)?;
        let mut root = xml::tree(&text, &part, &pkg.origin)?;
        let slides = listed(&self.root, "sldIdLst").count() as u64;
        let counts = [
            ("Slides", None, slides),
            ("Notes", Some(self.notes_added), 0),
            ("HiddenSlides", Some(self.hidden_added), 0),
        ];
        for (local, added, total) in counts {
            if let Some(n) = root.child_mut(PROPERTIES, local) {
                let base: u64 = n.text().trim().parse().unwrap_or(0);
                let count = added.map_or(total, |a| base + a);
                n.children = vec![Child::Text(count.to_string())];
            }
        }
        plan.replace(&part, Body::Data(part_bytes(&declaration(&text), &root)));
        Ok(())
    }

    fn finish(mut self, plan: &mut Plan, base: &mut Package) -> Result<(), Error> {
        self.write_sections();
        self.write_counts(plan, base)?;
        let lists = [&self.comment_authors, &self.authors, &self.table_styles];
        for list in lists.into_iter().flatten().filter(|l| l.changed) {
            let bytes = part_bytes(&list.declaration, &list.root);
            plan.replace(&list.part, Body::Data(bytes));
        }
        plan.replace(
            &self.main,
            Body::Data(part_bytes(&self.declaration, &self.root)),
        );
        rels_part(plan, base, &self.main, &self.rels);
        Ok(())
    }
}

/// A new GUID in braces, not yet in `used`, that depends only on the input and the old id.
fn free_guid(used: &mut HashSet<String>, input: usize, old: &str) -> String {
    (0u32..)
        .map(|attempt| format!("{{{}}}", guid((input, old, attempt))))
        .find(|id| used.insert(id.clone()))
        .expect("a free id")
}

/// The custom XML properties parts of a package.
fn item_parts(pkg: &Package) -> Vec<String> {
    let typed = |e: &&Entry| pkg.types.of(&e.name) == Some(ITEM_PROPERTIES);
    pkg.entries
        .iter()
        .filter(typed)
        .map(|e| e.name.clone())
        .collect()
}

/// An embedded font's typeface in lower case.
fn font_name(font: &Node) -> Option<String> {
    font.child(P, "font")?
        .attr("", "typeface")
        .map(str::to_lowercase)
}

/// Rewrites the copy of part `part` of input `i`.
fn edit_part(
    plan: &mut Plan,
    pkg: &mut Package,
    i: usize,
    part: &str,
    edit: impl FnOnce(&mut Node),
) -> Result<(), Error> {
    let text = pkg.text(part, MAX_PARSED)?;
    let mut root = xml::tree(&text, part, &pkg.origin)?;
    edit(&mut root);
    let name = plan.copied(i, part).expect("a copied part").to_string();
    plan.replace(&name, Body::Data(part_bytes(&declaration(&text), &root)));
    Ok(())
}

/// A whole part as bytes, after the declaration of the text it came from.
fn part_bytes(declaration: &str, root: &Node) -> Vec<u8> {
    let mut s = declaration.to_string();
    root.write(&mut s);
    s.into_bytes()
}

/// The PowerPoint rules of a merged deck: slide, master and layout ids unique and in range
/// (but for the `allowed` defects the base came with), each slide with one layout and at most
/// one notes page, and every slide of a section in the deck.
fn check(plan: &Plan, inputs: &mut [Package], allowed: usize) -> Result<(), Error> {
    let parts: HashMap<String, &Part> = plan
        .parts
        .iter()
        .map(|p| (p.name.to_ascii_lowercase(), p))
        .collect();
    let (origin, main) = (inputs[0].origin.clone(), inputs[0].main.clone());
    let mut text = |name: &str| -> Result<String, Error> {
        let get = |n: &str| parts.get(&n.to_ascii_lowercase());
        let part = get(name).or_else(|| get(&unescape(name)?));
        let text = part.map(|p| planned_text(p, inputs)).transpose()?.flatten();
        text.ok_or_else(|| Error::internal(&format!("part {name} is missing")))
    };
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
    let listed_ids: HashSet<&u32> = slide_ids.iter().collect();
    let sections = section_list(&root).into_iter().flat_map(Node::nodes);
    for list in sections.filter_map(|s| s.child(P14, "sldIdLst")) {
        let in_section = list.nodes().filter_map(|n| n.attr("", "id")?.parse().ok());
        if let Some(id) = { in_section }.find(|id: &u32| !listed_ids.contains(id)) {
            return Err(Error::internal(&format!(
                "a section lists a slide {id} that isn't there"
            )));
        }
    }
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
