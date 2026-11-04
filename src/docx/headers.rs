//! Headers and footers: no appended document shows the previous one's, and with even and odd
//! headers on, a document that had them off shows its default ones on even pages through
//! copies of those parts.

use quick_xml::events::{BytesStart, Event};

use super::{
    Merger, Parts, Place, REL, Rewrite, Uses, W, WML, Walk, registry, relative, start, story,
};
use crate::Origin;
use crate::error::Error;
use crate::package::{MAX_PARSED, RELS_TYPE, Rel, rels_name, resolve};
use crate::writer::{Body, R, rels_xml};
use crate::xml::{Attr, Node, Reader, XMLNS, declaration, start_tag};

/// What comes before `w:evenAndOddHeaders` in `w:settings`.
const BEFORE_EVEN: [&str; 38] = [
    "writeProtection",
    "view",
    "zoom",
    "removePersonalInformation",
    "removeDateAndTime",
    "doNotDisplayPageBoundaries",
    "displayBackgroundShape",
    "printPostScriptOverText",
    "printFractionalCharacterWidth",
    "printFormsData",
    "embedTrueTypeFonts",
    "embedSystemFonts",
    "saveSubsetFonts",
    "saveFormsData",
    "mirrorMargins",
    "alignBordersAndEdges",
    "bordersDoNotSurroundHeader",
    "bordersDoNotSurroundFooter",
    "gutterAtTop",
    "hideSpellingErrors",
    "hideGrammaticalErrors",
    "activeWritingStyle",
    "proofState",
    "formsDesign",
    "attachedTemplate",
    "linkStyles",
    "stylePaneFormatFilter",
    "stylePaneSortMethod",
    "documentType",
    "mailMerge",
    "revisionView",
    "trackRevisions",
    "doNotTrackMoves",
    "doNotTrackFormatting",
    "documentProtection",
    "autoFormatOverride",
    "styleLockTheme",
    "styleLockQFSet",
];

pub fn even_on(settings: &Node) -> bool {
    settings
        .child(W, "evenAndOddHeaders")
        .is_some_and(|n| !matches!(n.attr(W, "val"), Some("0" | "false" | "off")))
}

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

/// What a section gets besides its own children.
fn added(rw: &Rewrite, first: bool) -> String {
    let slots = rw.fill.iter().filter(|_| first);
    slots.map(|s| slot_tag(rw, s)).collect()
}

/// The section break of a document that has none.
pub fn missing_section(rw: &Rewrite, first: bool) -> Vec<u8> {
    let (w, more) = (&rw.w, added(rw, first));
    if more.is_empty() {
        return format!("<{w}:sectPr/>").into_bytes();
    }
    format!("<{w}:sectPr>{more}</{w}:sectPr>").into_bytes()
}

/// Where a section's children go, in the order they are written.
const REFERENCES: usize = 0;
const REST: usize = 1;
const LEFT_OUT: usize = 2;

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
/// each default one followed by its twin), then what the section gets, then the rest.
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
    if empty && more.is_empty() {
        return start(r, e, true, rw, place, &mut walk.bare, buf);
    }
    start(r, e, false, rw, place, &mut walk.bare, buf)?;
    let mut pieces: [Vec<u8>; 3] = Default::default();
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
                    at = REST;
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
                start(r, c, empty, rw, place, &mut walk.bare, &mut pieces[at])?;
                pieces[at].extend(twin.unwrap_or_default().as_bytes());
            }
            Event::End(_) if r.depth() < depth => {}
            Event::Eof => break,
            _ => super::put(&mut pieces[at], &event),
        }
    }
    let [references, rest, _] = pieces;
    buf.extend(references);
    buf.extend(more.as_bytes());
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

    /// Turns even and odd headers on in the base's settings.
    pub(super) fn even_setting(&mut self, origin: &Origin) -> Result<(), Error> {
        let main = self.main.clone();
        let own = self.inputs[0].rels_of(&main)?;
        let Some(mut h) = super::held(&mut self.inputs[0], &main, &own, "settings")? else {
            let w = crate::xml::prefix_of(&self.root.attrs, W).unwrap_or("w");
            let setting = format!("<{w}:evenAndOddHeaders/>");
            let parts = Parts {
                main: &main,
                kind: "settings",
            };
            let inserts: [(&[&str], &str); 1] = [(&[], &setting)];
            return registry(
                &mut self.plan,
                &mut self.rels,
                &parts,
                None,
                &self.root,
                &inserts,
                origin,
            );
        };
        h.tree
            .child_or_insert(W, "evenAndOddHeaders", &BEFORE_EVEN)
            .attrs
            .clear();
        let mut out = declaration(&h.text);
        h.tree.write(&mut out);
        self.plan.replace(&h.part, Body::Data(out.into_bytes()));
        Ok(())
    }
}
