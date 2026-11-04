//! Headers and footers: no appended document shows the previous one's, and with even and odd
//! headers on, a document that had them off shows its default ones on even pages through
//! copies of those parts.

use quick_xml::events::{BytesStart, Event};

use super::{Merger, Place, REL, Rewrite, Uses, W, WML, Walk, relative, start, story};
use crate::error::Error;
use crate::package::{MAX_PARSED, RELS_TYPE, Rel, rels_name, resolve};
use crate::writer::{Body, R, rels_xml};
use crate::xml::{Attr, Reader, XMLNS, start_tag};

/// The element's name when it is a header or footer reference.
pub fn reference(ns: &str, local: &str) -> Option<&'static str> {
    match local {
        "headerReference" if ns == W => Some("headerReference"),
        "footerReference" if ns == W => Some("footerReference"),
        _ => None,
    }
}

/// A reference an appended document's first section gets to a new empty part.
pub struct Slot {
    reference: &'static str,
    kind: &'static str,
    rid: String,
}

/// The `r:id` attribute of a new reference, with a declaration when the document has no
/// prefix for the namespace.
fn rid_attrs(rw: &Rewrite, rid: &str) -> Vec<Attr> {
    match &rw.r {
        Some(r) => vec![Attr::new(&format!("{r}:id"), R, rid)],
        None => vec![Attr::new("xmlns:r", XMLNS, R), Attr::new("r:id", R, rid)],
    }
}

fn slot_tag(rw: &Rewrite, slot: &Slot) -> String {
    let w = &rw.w;
    let mut attrs = vec![Attr::new(&format!("{w}:type"), W, slot.kind)];
    attrs.extend(rid_attrs(rw, &slot.rid));
    start_tag(&format!("{w}:{}", slot.reference), &attrs, true)
}

/// The references a section gets besides its own.
fn added(rw: &Rewrite, first: bool) -> String {
    let slots = rw.fill.iter().filter(|_| first);
    slots.map(|s| slot_tag(rw, s)).collect()
}

fn format_of(rw: &Rewrite, k: usize) -> &[u8] {
    rw.note_format[k].as_deref().unwrap_or_default().as_bytes()
}

/// The section break of a document that has none.
pub fn missing_section(rw: &Rewrite, first: bool) -> Vec<u8> {
    let (w, more) = (&rw.w, added(rw, first));
    let notes = [format_of(rw, 0), format_of(rw, 1)].concat();
    if more.is_empty() && notes.is_empty() {
        return format!("<{w}:sectPr/>").into_bytes();
    }
    let open = format!("<{w}:sectPr>{more}").into_bytes();
    [open, notes, format!("</{w}:sectPr>").into_bytes()].concat()
}

/// Where a section's children go, in the order they are written.
const REFERENCES: usize = 0;
const FOOTNOTES: usize = 1;
const ENDNOTES: usize = 2;
const REST: usize = 3;
const LEFT_OUT: usize = 4;

/// For a reference of a document whose even references go: whether this is one, and the twin
/// a default one is followed by.
fn twin_of(rw: &Rewrite, reference: &'static str, attrs: &[Attr]) -> (bool, Option<String>) {
    let kind = attrs.iter().find(|a| a.ns == W && a.local == "type");
    let kind = kind.map_or("default", |a| a.value.as_str());
    let old = attrs.iter().find(|a| a.ns == R && a.local == "id");
    let twin = old
        .and_then(|a| rw.twins.get(&a.value))
        .filter(|_| kind == "default");
    let slot = twin.map(|rid| Slot {
        reference,
        kind: "even",
        rid: rid.clone(),
    });
    (kind == "even", slot.map(|s| slot_tag(rw, &s)))
}

/// Writes a whole section break: its references first (with `twin` on, an even one left out and
/// each default one followed by its twin), then what the section gets, the document's note
/// formatting where the section has none, then the rest.
pub fn section<'a>(
    r: &mut Reader<'a>,
    e: &BytesStart<'a>,
    empty: bool,
    rw: &Rewrite,
    walk: &mut Walk,
    place: Place,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    let first = std::mem::take(&mut walk.first_sect);
    let more = added(rw, first);
    if empty && more.is_empty() && rw.note_format == [None, None] {
        return start(r, e, true, rw, place, walk, buf);
    }
    start(r, e, false, rw, place, walk, buf)?;
    let mut pieces: [Vec<u8>; 5] = Default::default();
    let depth = r.depth();
    let mut at = REST;
    while r.depth() >= depth && !empty {
        let event = r.event()?;
        match &event {
            Event::Start(c) | Event::Empty(c) => {
                let empty = matches!(event, Event::Empty(_));
                let child = r.depth() == depth + usize::from(!empty);
                let mut twin = None;
                if child {
                    at = match c.local_name().as_ref() {
                        "footnotePr" => FOOTNOTES,
                        "endnotePr" => ENDNOTES,
                        _ => REST,
                    };
                    if let Some(reference) = reference(r.element_ns(c), c.local_name().as_ref()) {
                        at = REFERENCES;
                        if rw.twin {
                            let (left_out, new) = twin_of(rw, reference, &r.attrs(c)?);
                            twin = new;
                            at = if left_out { LEFT_OUT } else { at };
                        }
                    }
                }
                let place = if child && first {
                    Place::FirstSection
                } else {
                    Place::Inner
                };
                start(r, c, empty, rw, place, walk, &mut pieces[at])?;
                pieces[at].extend(twin.unwrap_or_default().as_bytes());
            }
            Event::End(_) if r.depth() < depth => {}
            Event::Eof => break,
            _ => super::put(&mut pieces[at], &event),
        }
    }
    let [references, footnotes, endnotes, rest, _] = pieces;
    buf.extend(references);
    buf.extend(more.as_bytes());
    for (own, k) in [(footnotes, 0), (endnotes, 1)] {
        let format = format_of(rw, k);
        buf.extend(if own.is_empty() { format } else { &own });
    }
    buf.extend(rest);
    buf.extend(format!("</{}>", e.name().as_ref()).as_bytes());
    Ok(())
}

impl Merger<'_> {
    /// Copies each default header and footer of input `i`, to show on its even pages.
    pub(super) fn add_twins(
        &mut self,
        i: usize,
        own: &[Rel],
        uses: &Uses,
        rw: &mut Rewrite,
    ) -> Result<(), Error> {
        let part = self.inputs[i].main.clone();
        let origin = self.inputs[i].origin.clone();
        rw.twin = true;
        let body_rids = rw.rids.take();
        let appended = std::mem::replace(&mut rw.appended, true);
        rw.copying = true;
        for id in &uses.defaults {
            let Some(r) = own.iter().find(|r| &r.id == id) else {
                continue;
            };
            let target = resolve(&part, &r.target);
            let found = target.and_then(|t| self.inputs[i].find(&t).map(|e| e.name.clone()));
            let Some(source) = found.filter(|_| !rw.twins.contains_key(id)) else {
                continue;
            };
            let text = self.inputs[i].text(&source, MAX_PARSED)?;
            let bytes = story(&text, &source, &origin, rw)?;
            let shown = self.plan.copied(i, &source).unwrap_or(&source).to_string();
            let name = self.plan.claim(&shown);
            let kind = self.inputs[i].types.of(&source).unwrap_or_default();
            let kind = kind.to_string();
            self.plan.type_part(&name, &kind, false);
            let mut rels = self.inputs[i].rels_of(&source)?;
            for r in rels.iter_mut().filter(|r| !r.external) {
                let t = resolve(&source, &r.target);
                let t = t.and_then(|t| self.inputs[i].find(&t).map(|e| e.name.clone()));
                if let Some(t) = t {
                    r.target = relative(&name, self.plan.copied(i, &t).unwrap_or(&t));
                }
            }
            if !rels.is_empty() {
                let rels_out = rels_name(&name);
                self.plan.type_part(&rels_out, RELS_TYPE, true);
                self.plan.add(rels_out, Body::Data(rels_xml(&rels)), None);
            }
            self.plan.add(name.clone(), Body::Data(bytes), None);
            let new = self.rels.add(&r.kind, relative(&self.main, &name), false);
            rw.twins.insert(id.clone(), new);
        }
        rw.rids = body_rids;
        rw.appended = appended;
        rw.copying = false;
        Ok(())
    }

    /// Gives every slot an appended document's first section leaves open a new empty part, so
    /// it doesn't inherit the previous document's.
    pub(super) fn fill_slots(&mut self, uses: &Uses, rw: &mut Rewrite, twins: bool) {
        let w = rw.w.clone();
        for (reference, root, name) in [
            ("headerReference", "hdr", "header"),
            ("footerReference", "ftr", "footer"),
        ] {
            let has = |kind: &str| uses.first_slots.contains(&(reference, kind.to_string()));
            let props = self
                .out_styles
                .id_named("paragraph", name)
                .map_or(String::new(), |id| {
                    let style = Attr::new(&format!("{w}:val"), W, id);
                    let style = start_tag(&format!("{w}:pStyle"), &[style], true);
                    format!("<{w}:pPr>{style}</{w}:pPr>")
                });
            for kind in ["default", "first", "even"] {
                if has(kind) || (twins && kind == "even" && has("default")) {
                    continue;
                }
                let part = self.plan.claim(&format!("word/{name}1.xml"));
                self.plan
                    .type_part(&part, &format!("{WML}{name}+xml"), false);
                let xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n<{w}:{root} xmlns:{w}=\"{W}\"><{w}:p>{props}</{w}:p></{w}:{root}>"
                );
                self.plan
                    .add(part.clone(), Body::Data(xml.into_bytes()), None);
                let target = relative(&self.main, &part);
                let rid = self.rels.add(&format!("{REL}{name}"), target, false);
                rw.fill.push(Slot {
                    reference,
                    kind,
                    rid,
                });
            }
        }
    }
}
