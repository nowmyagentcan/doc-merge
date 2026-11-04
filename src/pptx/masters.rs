//! Slide masters as units: a master with all its layouts, its theme and what they relate to.
//! Two units are the same design when their canonical text is equal: every part in canonical
//! form, each relationship attribute replaced by the hash of what it points at, and media
//! compared by their bytes. Designs and slides from a deck of another size are fitted to the
//! output's.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::{Master, P, target};
use crate::error::Error;
use crate::package::{MAX_PARSED, Package};
use crate::writer::{MAX_SHARED, R, is_xml};
use crate::xml::{self, A, MC, Node};

/// The smallest font size a file may state, in hundredths of a point.
const MIN_FONT: i64 = 100;
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

/// How the content of a deck of another slide size fits the output's: scaled, then centred.
pub struct Fit {
    scale: f64,
    dx: i64,
    dy: i64,
}

impl Fit {
    /// None when the sizes are equal.
    pub fn new(from: (u64, u64), to: (u64, u64)) -> Option<Self> {
        if from == to {
            return None;
        }
        let side = |v: u64| v as f64;
        let scale = (side(to.0) / side(from.0)).min(side(to.1) / side(from.1));
        let margin = |to: u64, from: u64| ((side(to) - scale * side(from)) / 2.0).round() as i64;
        Some(Self {
            scale,
            dx: margin(to.0, from.0),
            dy: margin(to.1, from.1),
        })
    }

    fn scaled(&self, value: &str, least: i64) -> Option<String> {
        let v: i64 = value.parse().ok()?;
        let scaled = (v as f64 * self.scale).round() as i64;
        Some(scaled.max(least.min(v)).to_string())
    }

    /// Scales one attribute of `n` if it has it.
    fn scale_attr(&self, n: &mut Node, name: &str, least: i64) {
        let a = n
            .attrs
            .iter_mut()
            .find(|a| a.ns.is_empty() && a.local == name);
        if let Some(a) = a {
            a.value = self.scaled(&a.value, least).unwrap_or(a.value.clone());
        }
    }

    /// Fits a slide, layout or master.
    pub fn apply(&self, root: &mut Node) {
        root.walk_mut(&mut |n| {
            self.scale_attr(n, "defTabSz", i64::MIN);
            if n.ns != A {
                return;
            }
            let names: &[&str] = match n.local.as_str() {
                "off" | "chOff" => &["x", "y"],
                "ext" | "chExt" => &["cx", "cy"],
                "gridCol" => &["w"],
                "tr" => &["h"],
                "rPr" | "defRPr" | "endParaRPr" => {
                    self.scale_attr(n, "sz", MIN_FONT);
                    &[]
                }
                _ => &[],
            };
            for name in names {
                self.scale_attr(n, name, i64::MIN);
            }
        });
        let tree = root
            .child_mut(P, "cSld")
            .and_then(|c| c.child_mut(P, "spTree"));
        for shape in tree.into_iter().flat_map(Node::nodes_mut) {
            if shape.local != "nvGrpSpPr" && shape.local != "grpSpPr" {
                self.centre(shape);
            }
        }
    }

    /// Moves a top-level shape, whichever alternative of it a reader takes.
    fn centre(&self, shape: &mut Node) {
        if shape.ns == MC {
            for branch in shape.nodes_mut() {
                self.centre(branch);
            }
            return;
        }
        for n in shape.nodes_mut() {
            let holder = if n.local == "xfrm" {
                Some(n)
            } else if n.local == "spPr" || n.local == "grpSpPr" {
                n.child_mut(A, "xfrm")
            } else {
                None
            };
            let off = holder.and_then(|x| x.child_mut(A, "off"));
            for a in off.into_iter().flat_map(|o| o.attrs.iter_mut()) {
                let by = if a.local == "x" { self.dx } else { self.dy };
                if let Ok(v) = a.value.parse::<i64>() {
                    a.value = (v + by).to_string();
                }
            }
        }
    }
}

/// An aspect ratio in words, such as `4:3`.
pub fn ratio((cx, cy): (u64, u64)) -> String {
    let gcd = |mut a: u64, mut b: u64| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    let g = gcd(cx, cy);
    if cx / g > 100 || cy / g > 100 {
        return format!("{:.2}:1", cx as f64 / cy as f64);
    }
    format!("{}:{}", cx / g, cy / g)
}
