//! Footnotes, endnotes and comments: an appended document's items join the output's part with
//! new ids, and relationships inside them move into that part's relationships. The separator
//! notes stay the base's. The parts that say more about comments follow their paragraph ids,
//! and the people of every document are listed once.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::{
    Held, Parts, Rewrite, Uses, W, W15, copy_rel, extend, held, held_by, ids, registry, scan, story,
};
use crate::Origin;
use crate::error::Error;
use crate::package::{MAX_PARSED, Package, Rel};
use crate::writer::{Body, Plan, Rels, relative, rels_part};
use crate::xml::{self, Node, Prefixes, start_tag};

/// The notes parts, in the order [`Rewrite::notes`] holds their id maps.
pub const KINDS: [(&str, &str); 3] = [
    ("footnotes", "footnote"),
    ("endnotes", "endnote"),
    ("comments", "comment"),
];

/// Old to new ids of the items added, and of the paragraph ids that had to change.
pub type Added = (HashMap<String, String>, HashMap<u32, u32>);

/// One notes part of the output.
pub struct Notes {
    kind: &'static str,
    item: &'static str,
    base: Option<Held>,
    root: Prefixes,
    /// The output part's name, known before it is written.
    part: String,
    rels: Rels,
    rels_before: usize,
    next: i64,
    added: String,
    /// Whether an appended part stands in for one the base lacks, separators included.
    adopted: bool,
    /// Paragraph ids in the part.
    paras: HashSet<u32>,
    /// The ids of the items added.
    new_ids: HashSet<String>,
}

/// The ids of a notes part's items.
pub fn item_ids(h: &Held) -> HashSet<String> {
    let ids = h.tree.nodes().filter_map(|n| n.attr(W, "id"));
    ids.map(String::from).collect()
}

fn number(n: &xml::Node) -> i64 {
    n.attr(W, "id").and_then(|v| v.parse().ok()).unwrap_or(0)
}

impl Notes {
    pub fn open(
        pkg: &mut Package,
        main: &str,
        rels: &[Rel],
        (kind, item): (&'static str, &'static str),
    ) -> Result<Self, Error> {
        let base = held(pkg, main, rels, kind)?;
        let part = base
            .as_ref()
            .map_or(format!("word/{kind}.xml"), |h| h.part.clone());
        let own = Rels::new(pkg.rels_of(&part)?);
        let first = i64::from(kind != "comments");
        let highest = base.iter().flat_map(|h| h.tree.nodes()).map(number).max();
        Ok(Self {
            kind,
            item,
            root: Prefixes::of(base.as_ref().map_or(&[], |h| &h.tree.attrs)),
            base,
            part,
            rels_before: own.list.len(),
            rels: own,
            next: highest.map_or(first, |m| first.max(m + 1)),
            added: String::new(),
            adopted: false,
            paras: HashSet::new(),
            new_ids: HashSet::new(),
        })
    }

    /// The ids of every item in the part so far.
    pub fn ids(&self) -> HashSet<String> {
        let mut ids = self.base.as_ref().map_or(HashSet::new(), item_ids);
        ids.extend(self.new_ids.iter().cloned());
        ids
    }

    /// What the base's part uses.
    pub fn scan_base(&mut self, origin: &Origin) -> Result<Uses, Error> {
        let Some(h) = &self.base else {
            return Ok(Uses::default());
        };
        let uses = scan(&h.text, &h.part, origin, false)?;
        self.paras = uses.paras.iter().copied().collect();
        Ok(uses)
    }

    /// Adds input `i`'s items.
    pub fn add(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        doc: &Held,
        rw: &mut Rewrite,
    ) -> Result<Added, Error> {
        let origin = inputs[i].origin.clone();
        let own = inputs[i].rels_of(&doc.part)?;
        let mut rids = HashMap::new();
        let uses = scan(&doc.text, &doc.part, &origin, false)?;
        rw.paras = ids::new_values(i + 1, &uses.paras, &mut self.paras);
        for id in uses.rids {
            let Some(r) = own.iter().find(|r| r.id == id) else {
                let why = format!("{} points to a missing relationship {id}", doc.part);
                return Err(origin.bad(&why));
            };
            let (new, _) = copy_rel(plan, inputs, i, &doc.part, r, &self.part, &mut self.rels)?;
            rids.insert(id, new);
        }
        let body_rids = rw.rids.replace(rids);
        let text = story(&doc.text, &doc.part, &origin, rw);
        rw.rids = body_rids;
        let paras = std::mem::take(&mut rw.paras);
        let text = String::from_utf8(text?).map_err(|e| Error::internal(&e.to_string()))?;
        let tree = xml::tree(&text, &doc.part, &origin)?;
        let local = self.root.merge(&tree.attrs);
        let adopt = self.base.is_none() && !self.adopted;
        self.adopted = true;
        let items = || tree.nodes().filter(|n| n.is(W, self.item));
        let typed = |n: &&xml::Node| n.attr(W, "type").is_some_and(|t| t != "normal");
        if adopt && let Some(m) = items().filter(typed).map(number).max() {
            self.next = self.next.max(m + 1);
        }
        let mut ids = HashMap::new();
        let mut added = String::new();
        let mut next = self.next;
        for n in items() {
            if typed(&n) && !adopt {
                continue;
            }
            let mut n = n.clone();
            if !typed(&&n) {
                ids.insert(number(&n).to_string(), next.to_string());
                n.set_attr(W, "w", "id", &next.to_string());
                next += 1;
            }
            self.new_ids.extend(n.attr(W, "id").map(String::from));
            n.attrs.extend(local.iter().cloned());
            n.write(&mut added);
        }
        self.next = next;
        self.added += &added;
        Ok((ids, paras))
    }

    /// Writes the part when anything was added.
    pub fn finish(
        &self,
        plan: &mut Plan,
        rels: &mut Rels,
        base: &Package,
        main: &str,
        origin: &Origin,
    ) -> Result<(), Error> {
        let parts = Parts {
            main,
            kind: self.kind,
            root: self.kind,
        };
        let inserts: [(&[&str], &str); 1] = [(&[], &self.added)];
        registry(
            plan,
            rels,
            &parts,
            self.base.as_ref(),
            &self.root,
            &inserts,
            origin,
        )?;
        if self.rels.list.len() > self.rels_before {
            rels_part(plan, base, &self.part, &self.rels);
        }
        Ok(())
    }
}

/// The parts beside the comments, by relationship type.
const SIDES: [&str; 4] = [
    "http://schemas.microsoft.com/office/2011/relationships/commentsExtended",
    "http://schemas.microsoft.com/office/2016/09/relationships/commentsIds",
    "http://schemas.microsoft.com/office/2018/08/relationships/commentsExtensible",
    "http://schemas.microsoft.com/office/2011/relationships/people",
];

/// One such part of the output.
struct Side {
    kind: &'static str,
    base: Option<Held>,
    root: Prefixes,
    /// What a part standing in for one the base lacks is called: its name, root element and
    /// content type.
    adopted: Option<(String, String, String)>,
    added: String,
}

/// The output's parts beside the comments.
pub struct Sides {
    sides: Vec<Side>,
    durable_ids: HashSet<u32>,
    people: HashSet<String>,
}

fn hex(n: &Node, local: &str) -> Vec<u32> {
    let values = n.attrs.iter().filter(|a| a.local == local);
    values
        .filter_map(|a| u32::from_str_radix(&a.value, 16).ok())
        .collect()
}

fn durable_ids(held: &[Option<Held>]) -> BTreeSet<u32> {
    let items = held.iter().flatten().flat_map(|h| h.tree.nodes());
    items.flat_map(|n| hex(n, "durableId")).collect()
}

fn author(n: &Node) -> Option<&str> {
    n.attr(W15, "author")
}

fn read(pkg: &mut Package, main: &str, rels: &[Rel]) -> Result<Vec<Option<Held>>, Error> {
    let found = SIDES
        .iter()
        .map(|kind| held_by(pkg, main, rels, kind, MAX_PARSED));
    found.collect()
}

impl Sides {
    pub fn open(pkg: &mut Package, main: &str, rels: &[Rel]) -> Result<Self, Error> {
        let mut held = read(pkg, main, rels)?;
        let durable_ids = durable_ids(&held).into_iter().collect();
        let people = held[3].iter().flat_map(|h| h.tree.nodes());
        let people = people.filter_map(author).map(String::from).collect();
        let sides = SIDES.iter().zip(&mut held).map(|(kind, h)| {
            let base = h.take();
            Side {
                kind,
                root: Prefixes::of(base.as_ref().map_or(&[], |h| &h.tree.attrs)),
                base,
                adopted: None,
                added: String::new(),
            }
        });
        Ok(Self {
            sides: sides.collect(),
            durable_ids,
            people,
        })
    }

    /// Adds input `i`'s entries, with `paras` the new paragraph ids of its comments.
    pub fn add(
        &mut self,
        pkg: &mut Package,
        rels: &[Rel],
        paras: &HashMap<u32, u32>,
    ) -> Result<(), Error> {
        let main = pkg.main.clone();
        let held = read(pkg, &main, rels)?;
        let input = pkg.origin.input;
        let durable = ids::new_values(input, &durable_ids(&held), &mut self.durable_ids);
        for (side, h) in self.sides.iter_mut().zip(&held) {
            let Some(h) = h else { continue };
            if side.base.is_none() && side.adopted.is_none() {
                let kind = pkg.types.of(&h.part).unwrap_or_default().to_string();
                side.adopted = Some((h.part.clone(), h.tree.name.clone(), kind));
                side.root = Prefixes::of(&h.tree.attrs);
            }
            let local = side.root.merge(&h.tree.attrs);
            for n in h.tree.nodes() {
                if author(n).is_some_and(|a| !self.people.insert(a.to_string())) {
                    continue;
                }
                let mut n = n.clone();
                for a in &mut n.attrs {
                    let map = match a.local.as_str() {
                        "paraId" | "paraIdParent" => paras,
                        "durableId" => &durable,
                        _ => continue,
                    };
                    let old = u32::from_str_radix(&a.value, 16).ok();
                    if let Some(new) = old.and_then(|v| map.get(&v)) {
                        a.value = format!("{new:08X}");
                    }
                }
                n.attrs.extend(local.iter().cloned());
                n.write(&mut side.added);
            }
        }
        Ok(())
    }

    /// Writes the parts that gained anything.
    pub fn finish(
        &self,
        plan: &mut Plan,
        rels: &mut Rels,
        main: &str,
        origin: &Origin,
    ) -> Result<(), Error> {
        for side in self.sides.iter().filter(|s| !s.added.is_empty()) {
            if let Some(h) = &side.base {
                let inserts: [(&[&str], &str); 1] = [(&[], &side.added)];
                let bytes = extend(&h.text, &h.part, origin, &side.root.attrs, &inserts)?;
                plan.replace(&h.part, Body::Data(bytes));
            } else if let Some((part, root, kind)) = &side.adopted {
                let mut text = String::from(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n",
                );
                text += &start_tag(root, &side.root.attrs, false);
                text += &side.added;
                text += &format!("</{root}>");
                let part = plan.claim(part);
                plan.type_part(&part, kind, false);
                rels.add(side.kind, relative(main, &part), false);
                plan.add(part, Body::Data(text.into_bytes()), None);
            }
        }
        Ok(())
    }
}
