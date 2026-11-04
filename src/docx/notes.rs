//! Footnotes, endnotes and comments: an appended document's items join the output's part with
//! new ids, and relationships inside them move into that part's relationships. The separator
//! notes stay the base's.

use std::collections::HashMap;

use super::{Held, Parts, Rewrite, W, copy_rel, held, registry, scan, story};
use crate::Origin;
use crate::error::Error;
use crate::package::{Package, Rel};
use crate::writer::{Plan, Rels, rels_part};
use crate::xml::{self, Prefixes};

/// The notes parts, in the order [`Rewrite::notes`] holds their id maps.
pub const KINDS: [(&str, &str); 3] = [
    ("footnotes", "footnote"),
    ("endnotes", "endnote"),
    ("comments", "comment"),
];

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
        })
    }

    /// Adds input `i`'s items; returns their old to new ids.
    pub fn add(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        doc: &Held,
        rw: &mut Rewrite,
    ) -> Result<HashMap<String, String>, Error> {
        let origin = inputs[i].origin.clone();
        let own = inputs[i].rels_of(&doc.part)?;
        let mut rids = HashMap::new();
        for id in scan(&doc.text, &doc.part, &origin, false)?.rids {
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
        let text = String::from_utf8(text?).map_err(|e| Error::internal(&e.to_string()))?;
        let tree = xml::tree(&text, &doc.part, &origin)?;
        let local = self.root.merge(&tree.attrs);
        let adopt = self.base.is_none() && !self.adopted;
        self.adopted = true;
        let items = || tree.nodes().filter(|n| n.is(W, self.item));
        let typed = |n: &&xml::Node| n.attr(W, "type").is_some();
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
            n.attrs.extend(local.iter().cloned());
            n.write(&mut added);
        }
        self.next = next;
        self.added += &added;
        Ok(ids)
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
