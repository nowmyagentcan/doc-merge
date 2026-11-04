//! Slide masters as units: a master with all its layouts, its theme and what they relate to.
//! Two units are the same design when their canonical text is equal: every part in canonical
//! form, each relationship attribute replaced by the hash of what it points at, and media
//! compared by their bytes.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::{Master, P, target};
use crate::error::Error;
use crate::package::{MAX_PARSED, Package};
use crate::writer::{MAX_SHARED, R, is_xml};
use crate::xml;

/// The hash of a target already on the walk's stack, such as a layout's link to its master.
const CYCLE: u64 = 0;

fn hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// A unit's canonical text and the parts compared by their bytes, by size and CRC.
pub struct Design {
    text: String,
    media: Vec<(u64, u32, String)>,
}

/// One walk over a unit: each part once.
struct Walk<'a> {
    pkg: &'a mut Package,
    stack: Vec<String>,
    hashes: HashMap<String, u64>,
    texts: Vec<String>,
    media: Vec<(u64, u32, String)>,
}

impl Walk<'_> {
    /// Adds a part's canonical text and returns its hash.
    fn part(&mut self, part: &str) -> Result<u64, Error> {
        if self.stack.iter().any(|p| p == part) {
            return Ok(CYCLE);
        }
        if let Some(h) = self.hashes.get(part) {
            return Ok(*h);
        }
        let content_type = self.pkg.types.of(part).unwrap_or_default().to_string();
        let text = if is_xml(&content_type) {
            self.xml(part)?
        } else {
            let e = self.pkg.find(part).expect("a part of the input");
            let (size, crc) = (e.size, e.crc);
            self.media.push((size, crc, part.to_string()));
            format!("{content_type} {size} {crc:08x}")
        };
        let h = hash(&text);
        self.hashes.insert(part.to_string(), h);
        self.texts.push(text);
        Ok(h)
    }

    fn xml(&mut self, part: &str) -> Result<String, Error> {
        self.stack.push(part.to_string());
        let rels = self.pkg.rels_of(part)?;
        let mut targets = HashMap::new();
        let mut links = vec![];
        for r in &rels {
            let h = if r.external {
                hash(&r.target)
            } else {
                let to = target(self.pkg, part, &rels, &r.id)?;
                self.part(&to)?
            };
            targets.insert(r.id.as_str(), h);
            links.push(format!("{} {h:016x}", r.kind));
        }
        self.stack.pop();
        let text = self.pkg.text(part, MAX_PARSED)?;
        let mut tree = xml::tree(&text, part, &self.pkg.origin)?;
        tree.walk_mut(&mut |n| {
            let layout = n.is(P, "sldLayoutId");
            n.attrs.retain(|a| {
                !(a.ns.is_empty() && (a.local == "dirty" || a.local == "smtClean"))
                    && !(layout && a.ns.is_empty() && a.local == "id")
            });
            for a in n.attrs.iter_mut().filter(|a| a.ns == R) {
                if let Some(h) = targets.get(a.value.as_str()) {
                    a.value = format!("{h:016x}");
                }
            }
        });
        let mut form = String::new();
        tree.canonical(&mut form);
        links.sort();
        Ok(format!("{form}\n{}", links.join("\n")))
    }
}

impl Design {
    pub fn of(pkg: &mut Package, master: &Master) -> Result<Self, Error> {
        let mut walk = Walk {
            pkg,
            stack: vec![],
            hashes: HashMap::new(),
            texts: vec![],
            media: vec![],
        };
        walk.part(&master.part)?;
        let (mut texts, mut media) = (walk.texts, walk.media);
        texts.sort();
        media.sort();
        Ok(Self {
            text: texts.join("\n"),
            media,
        })
    }
}

/// A unit in the output.
pub struct Unit {
    pub master: String,
    pub layouts: Vec<String>,
    input: usize,
    media: Vec<(u64, u32, String)>,
}

/// The units in the output by slide size and canonical text.
#[derive(Default)]
pub struct Units(HashMap<(u64, u64, String), Vec<Unit>>);

impl Units {
    pub fn add(
        &mut self,
        size: (u64, u64),
        design: Design,
        input: usize,
        master: String,
        layouts: Vec<String>,
    ) {
        let unit = Unit {
            master,
            layouts,
            input,
            media: design.media,
        };
        let key = (size.0, size.1, design.text);
        self.0.entry(key).or_default().push(unit);
    }

    /// The unit in the output that is this design in a deck of this slide size, if any.
    pub fn find(
        &self,
        size: (u64, u64),
        design: &Design,
        input: usize,
        inputs: &mut [Package],
    ) -> Result<Option<&Unit>, Error> {
        let key = (size.0, size.1, design.text.clone());
        for unit in self.0.get(&key).into_iter().flatten() {
            if same_media(unit, design, input, inputs)? {
                return Ok(Some(unit));
            }
        }
        Ok(None)
    }
}

fn same_media(
    unit: &Unit,
    design: &Design,
    input: usize,
    inputs: &mut [Package],
) -> Result<bool, Error> {
    for (theirs, ours) in unit.media.iter().zip(&design.media) {
        if theirs.0 > MAX_SHARED {
            return Ok(false);
        }
        let bytes = inputs[unit.input].read(&theirs.2, MAX_SHARED)?;
        if bytes != inputs[input].read(&ours.2, MAX_SHARED)? {
            return Ok(false);
        }
    }
    Ok(true)
}
