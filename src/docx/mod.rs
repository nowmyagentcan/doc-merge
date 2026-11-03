//! Word: the bodies joined in order, every appended document starting on a new page with its
//! own sections, its styles mapped or copied (`styles`), and the parts its content points to
//! copied with new relationship ids.

pub mod styles;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;

use quick_xml::events::{BytesStart, Event};

use crate::error::{Error, Warnings};
use crate::package::{MAX_PARSED, MAX_STREAMED, Package, RELS_TYPE, Rel, rels_name, resolve};
use crate::writer::{Body, Plan, R, relative, rels_xml};
use crate::xml::{self, Attr, MC, Node, Reader, XMLNS, start_tag};
use crate::{Origin, Rename, StyleMode};
use styles::{Sheet, Source};

pub const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const O: &str = "urn:schemas-microsoft-com:office:office";
const DSP: &str = "http://schemas.microsoft.com/office/drawing/2008/diagram";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const DIAGRAM_DRAWING: &str =
    "http://schemas.microsoft.com/office/2007/relationships/diagramDrawing";
const WML: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.";

/// What a merge of Word documents gives besides the plan.
pub struct Merged {
    pub plan: Plan,
    /// Each input's section count.
    pub sections: Vec<usize>,
    pub renames: Vec<Rename>,
}

/// A part a document points to by relationship type: its name, text and tree.
struct Held {
    part: String,
    text: String,
    tree: Node,
}

fn held(pkg: &mut Package, main: &str, rels: &[Rel], kind: &str) -> Result<Option<Held>, Error> {
    let wanted = format!("{REL}{kind}");
    let Some(r) = rels.iter().find(|r| r.kind == wanted && !r.external) else {
        return Ok(None);
    };
    let part = resolve(main, &r.target).and_then(|t| pkg.find(&t).map(|e| e.name.clone()));
    let Some(part) = part else {
        return Err(pkg
            .origin
            .bad(&format!("{main} points to a missing part {}", r.target)));
    };
    let text = pkg.text(&part, MAX_PARSED)?;
    let tree = xml::tree(&text, &part, &pkg.origin)?;
    Ok(Some(Held { part, text, tree }))
}

pub fn merge(
    inputs: &mut [Package],
    mode: StyleMode,
    warnings: &mut Warnings,
) -> Result<Merged, Error> {
    let mut plan = Plan::base(&inputs[0], warnings)?;
    let main = inputs[0].main.clone();
    let base_rels = inputs[0].rels_of(&main)?;
    let mut rels = Rels::new(base_rels.clone());
    let base_styles = held(&mut inputs[0], &main, &base_rels, "styles")?;
    let mut style_root = Prefixes::of(base_styles.as_ref().map_or(&[], |h| &h.tree.attrs));
    let mut out_styles = styles::Output::new(Sheet::new(base_styles.as_ref().map(|h| &h.tree)));
    let mut docs = vec![];
    let mut sections = vec![];
    let mut renames = vec![];
    let mut root = Prefixes::default();
    for i in 0..inputs.len() {
        let part = inputs[i].main.clone();
        let origin = inputs[i].origin.clone();
        let text = inputs[i].text(&part, MAX_STREAMED)?;
        let mut uses = scan(&text, &part, &origin)?;
        drop(text);
        sections.push(uses.sections);
        let w = prefix_of(&uses.root, W).unwrap_or("w").to_string();
        if i == 0 {
            root = Prefixes::of(&uses.root);
            docs.push(Rewrite {
                w,
                ..Rewrite::default()
            });
            continue;
        }
        let own = inputs[i].rels_of(&part)?;
        let mut stories = vec![];
        for id in &uses.rids {
            let Some(r) = own.iter().find(|r| &r.id == id) else {
                return Err(origin.bad(&format!("{part} points to a missing relationship {id}")));
            };
            if r.external || ![format!("{REL}header"), format!("{REL}footer")].contains(&r.kind) {
                continue;
            }
            let Some(target) = resolve(&part, &r.target) else {
                continue;
            };
            if inputs[i].has(&target) {
                let text = inputs[i].text(&target, MAX_PARSED)?;
                let found = scan(&text, &target, &origin)?;
                uses.styles.extend(found.styles);
                stories.push((id.clone(), target, text));
            }
        }
        let doc_styles = held(&mut inputs[i], &part, &own, "styles")?;
        let sheet = Sheet::new(doc_styles.as_ref().map(|h| &h.tree));
        let plain = |n: &str| n.to_string();
        let source = Source {
            sheet: &sheet,
            mode,
            position: i + 1,
            lists: &plain,
            output_lists: &plain,
        };
        let mut seeds = uses.styles.clone();
        seeds.extend(
            ["paragraph", "table"]
                .iter()
                .filter_map(|k| sheet.default_of(k))
                .map(String::from),
        );
        let mapped = out_styles.map(&source, &sheet.closure(&seeds, &|_| vec![]))?;
        renames.extend(mapped.renames.iter().map(|(from, to)| Rename {
            input: i + 1,
            kind: "style",
            from: from.clone(),
            to: to.clone(),
        }));
        let mut rw = Rewrite {
            rids: Some(HashMap::new()),
            styles: mapped.ids,
            local: root.merge(&uses.root),
            w,
            appended: true,
        };
        let local = style_root.merge(doc_styles.as_ref().map_or(&[], |h| &h.tree.attrs));
        for s in &mut out_styles.styles[mapped.copied] {
            s.walk_mut(&mut |n| {
                rw.attrs(&n.ns, &n.local, &mut n.attrs);
            });
            s.attrs.extend(local.iter().cloned());
        }
        let mut done = HashSet::new();
        for id in &uses.rids {
            let Some(r) = own.iter().find(|r| &r.id == id) else {
                continue;
            };
            let (new, copied) = copy_rel(&mut plan, inputs, i, &part, r, &main, &mut rels)?;
            rw.rids.as_mut().unwrap().insert(id.clone(), new);
            if let Some((_, source, text)) = stories.iter().find(|s| &s.0 == id)
                && let Some(copied) = copied
                && done.insert(copied.clone())
            {
                let body = story(text, source, &origin, &rw)?;
                plan.replace(&copied, Body::Data(body));
            }
        }
        docs.push(rw);
    }
    let added: String = out_styles.added().iter().fold(String::new(), |mut s, n| {
        n.write(&mut s);
        s
    });
    let parts = Parts {
        main: &main,
        kind: "styles",
        root_name: "styles",
    };
    let base_held = base_styles
        .as_ref()
        .map(|h| (h.part.as_str(), h.text.as_str()));
    let origin = inputs[0].origin.clone();
    let inserts: [(&[&str], &str); 1] = [(&[], &added)];
    registry(
        &mut plan,
        &mut rels,
        &parts,
        base_held,
        &style_root,
        &inserts,
        &origin,
    )?;
    let root_attrs = root.attrs;
    let count = inputs.len();
    plan.replace(
        &main,
        Body::Stream(Box::new(move |out, inputs| {
            let mut tail = String::new();
            for (i, rw) in docs.iter().enumerate() {
                let part = inputs[i].main.clone();
                let origin = inputs[i].origin.clone();
                let text = inputs[i].text(&part, MAX_STREAMED)?;
                let role = Role {
                    base: i == 0,
                    last: i + 1 == count,
                    root: &root_attrs,
                };
                tail = body(&text, &part, &origin, rw, &role, out)?.unwrap_or(tail);
            }
            out.write_all(tail.as_bytes()).map_err(io)
        })),
    );
    let doc_rels = rels_name(&main);
    if inputs[0].has(&doc_rels) {
        plan.replace(&doc_rels, Body::Data(rels_xml(&rels.list)));
    } else {
        plan.type_part(&doc_rels, RELS_TYPE, true);
        plan.add(doc_rels, Body::Data(rels_xml(&rels.list)), None);
    }
    Ok(Merged {
        plan,
        sections,
        renames,
    })
}

/// Which registry part is being written.
struct Parts<'a> {
    main: &'a str,
    /// The relationship type's last word, also the part's file stem and content type word.
    kind: &'a str,
    root_name: &'a str,
}

/// Writes a registry part with `inserts` (see [`extend`]) when any has content: the base's
/// part extended, or a new part when the base has none.
fn registry(
    plan: &mut Plan,
    rels: &mut Rels,
    parts: &Parts,
    base: Option<(&str, &str)>,
    root: &Prefixes,
    inserts: &[(&[&str], &str)],
    origin: &Origin,
) -> Result<(), Error> {
    if inserts.iter().all(|i| i.1.is_empty()) {
        return Ok(());
    }
    if let Some((part, text)) = base {
        let bytes = extend(text, part, origin, &root.attrs, inserts)?;
        plan.replace(part, Body::Data(bytes));
        return Ok(());
    }
    let w = prefix_of(&root.attrs, W).unwrap_or("w");
    let name = format!("{w}:{}", parts.root_name);
    let mut text =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n");
    text += &start_tag(&name, &root.attrs, false);
    for (_, content) in inserts {
        text += content;
    }
    text += &format!("</{name}>");
    let part = plan.claim(&format!("word/{}.xml", parts.kind));
    plan.type_part(&part, &format!("{WML}{}+xml", parts.kind), false);
    let target = relative(parts.main, &part);
    rels.add(&format!("{REL}{}", parts.kind), target, false);
    plan.add(part, Body::Data(text.into_bytes()), None);
    Ok(())
}

/// A registry part with its root's attributes replaced and new top-level content added. Each
/// insert goes before the first top-level element it names, or at the end of the root.
fn extend(
    text: &str,
    part: &str,
    origin: &Origin,
    root: &[Attr],
    inserts: &[(&[&str], &str)],
) -> Result<Vec<u8>, Error> {
    let mut r = Reader::new(text, part, origin);
    let mut buf = Vec::with_capacity(text.len());
    let mut next = 0;
    loop {
        let event = r.event()?;
        match &event {
            Event::Eof => return Ok(buf),
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                let depth = r.depth() - usize::from(!empty);
                if depth == 0 {
                    buf.extend(start_tag(e.name().as_ref(), root, false).as_bytes());
                    if empty {
                        for (_, content) in inserts {
                            buf.extend(content.as_bytes());
                        }
                        buf.extend(format!("</{}>", e.name().as_ref()).as_bytes());
                    }
                    continue;
                }
                if depth == 1 {
                    let local = e.local_name();
                    while next < inserts.len() && inserts[next].0.contains(&local.as_ref()) {
                        buf.extend(inserts[next].1.as_bytes());
                        next += 1;
                    }
                }
                put(&mut buf, &event);
            }
            Event::End(_) => {
                if r.depth() == 0 {
                    for (_, content) in &inserts[next..] {
                        buf.extend(content.as_bytes());
                    }
                }
                put(&mut buf, &event);
            }
            _ => put(&mut buf, &event),
        }
    }
}

/// A header or footer with the rewrite applied.
fn story(text: &str, part: &str, origin: &Origin, rw: &Rewrite) -> Result<Vec<u8>, Error> {
    let mut r = Reader::new(text, part, origin);
    let mut buf = Vec::with_capacity(text.len());
    loop {
        let event = r.event()?;
        match &event {
            Event::Eof => return Ok(buf),
            Event::Start(e) => start(&r, e, false, rw, false, false, &mut buf)?,
            Event::Empty(e) => start(&r, e, true, rw, false, false, &mut buf)?,
            _ => put(&mut buf, &event),
        }
    }
}

fn io(e: std::io::Error) -> Error {
    Error::write_failed("the merged file", &e)
}

/// A part's relationships as the output will have them, with new ids after the highest.
pub struct Rels {
    pub list: Vec<Rel>,
    next: u32,
}

impl Rels {
    pub fn new(list: Vec<Rel>) -> Self {
        let next = list
            .iter()
            .filter_map(|r| r.id.strip_prefix("rId")?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        Self { list, next }
    }

    /// Adds a relationship and returns its new id.
    pub fn add(&mut self, kind: &str, target: String, external: bool) -> String {
        let id = loop {
            let id = format!("rId{}", self.next);
            self.next += 1;
            if !self.list.iter().any(|r| r.id == id) {
                break id;
            }
        };
        self.list.push(Rel {
            id: id.clone(),
            kind: kind.into(),
            target,
            external,
        });
        id
    }
}

/// Copies what relationship `r` of `part` (in input `i`) points to into the output, and adds a
/// relationship to it from the output's part `to`. Returns the new id and the copy's name.
pub fn copy_rel(
    plan: &mut Plan,
    inputs: &mut [Package],
    i: usize,
    part: &str,
    r: &Rel,
    to: &str,
    rels: &mut Rels,
) -> Result<(String, Option<String>), Error> {
    if r.external {
        return Ok((rels.add(&r.kind, r.target.clone(), true), None));
    }
    let target = resolve(part, &r.target).and_then(|t| inputs[i].find(&t).map(|e| e.name.clone()));
    let Some(target) = target else {
        let why = format!("{part} points to a missing part {}", r.target);
        return Err(inputs[i].origin.bad(&why));
    };
    let copied = plan.copy_graph(inputs, i, &target, &|_, _| None)?;
    if r.kind == format!("{REL}diagramData") {
        diagram_drawing(plan, inputs, i, part, &target, &copied, to, rels)?;
    }
    let id = rels.add(&r.kind, relative(to, &copied), false);
    Ok((id, Some(copied)))
}

/// A SmartArt diagram's data names its drawing by a relationship id of the part that shows the
/// diagram, so the drawing comes along and the data part gets the new id.
#[allow(clippy::too_many_arguments)]
fn diagram_drawing(
    plan: &mut Plan,
    inputs: &mut [Package],
    i: usize,
    part: &str,
    data: &str,
    copied: &str,
    to: &str,
    rels: &mut Rels,
) -> Result<(), Error> {
    let origin = inputs[i].origin.clone();
    let text = inputs[i].text(data, MAX_PARSED)?;
    let mut tree = xml::tree(&text, data, &origin)?;
    let own = inputs[i].rels_of(part)?;
    let mut changed = false;
    let mut found = vec![];
    tree.walk(&mut |n| {
        if n.is(DSP, "dataModelExt")
            && let Some(id) = n.attr("", "relId")
        {
            found.push(id.to_string());
        }
    });
    let mut map = HashMap::new();
    for id in found {
        let Some(r) = own.iter().find(|r| r.id == id && r.kind == DIAGRAM_DRAWING) else {
            continue;
        };
        let (new, _) = copy_rel(plan, inputs, i, part, &r.clone(), to, rels)?;
        map.insert(id, new);
    }
    tree.walk_mut(&mut |n| {
        if n.is(DSP, "dataModelExt")
            && let Some(a) = n
                .attrs
                .iter_mut()
                .find(|a| a.ns.is_empty() && a.local == "relId")
            && let Some(new) = map.get(&a.value)
        {
            a.value = new.clone();
            changed = true;
        }
    });
    if changed {
        let mut out = declaration(&text);
        tree.write(&mut out);
        plan.replace(copied, Body::Data(out.into_bytes()));
    }
    Ok(())
}

/// The XML declaration a part starts with, if any, with its line break.
pub fn declaration(text: &str) -> String {
    match text.find("?>") {
        Some(end) if text.starts_with("<?xml") => {
            let rest = &text[end + 2..];
            let eol = rest.len() - rest.trim_start_matches(['\r', '\n']).len();
            text[..end + 2 + eol].to_string()
        }
        _ => String::new(),
    }
}

/// The prefix a root element binds to `ns`.
pub fn prefix_of<'a>(root: &'a [Attr], ns: &str) -> Option<&'a str> {
    root.iter()
        .find(|a| a.is_declaration() && a.value == ns && !a.local.is_empty())
        .map(|a| a.local.as_str())
}

/// The namespace declarations and `mc:Ignorable` prefixes a merged part's root element carries:
/// the base's, plus those of appended content whose prefixes the base doesn't use.
#[derive(Default)]
pub struct Prefixes {
    pub attrs: Vec<Attr>,
}

impl Prefixes {
    pub fn of(root: &[Attr]) -> Self {
        Self {
            attrs: root.to_vec(),
        }
    }

    /// Takes in the declarations of an appended part's root; returns those whose prefix the
    /// output binds to another namespace, which each copied element then declares itself.
    pub fn merge(&mut self, root: &[Attr]) -> Vec<Attr> {
        let mut local = vec![];
        for d in root.iter().filter(|a| a.is_declaration()) {
            match self
                .attrs
                .iter()
                .find(|a| a.is_declaration() && a.local == d.local)
            {
                Some(a) if a.value == d.value => {}
                Some(_) => local.push(d.clone()),
                None => self.attrs.push(d.clone()),
            }
        }
        let ignorable = root.iter().find(|a| a.ns == MC && a.local == "Ignorable");
        let wanted: Vec<&str> = ignorable
            .map(|a| a.value.split_whitespace().collect())
            .unwrap_or_default();
        let wanted: Vec<String> = wanted
            .into_iter()
            .filter(|p| !local.iter().any(|d| d.local == *p))
            .map(String::from)
            .collect();
        if wanted.is_empty() {
            return local;
        }
        match self
            .attrs
            .iter_mut()
            .find(|a| a.ns == MC && a.local == "Ignorable")
        {
            Some(a) => {
                for p in wanted {
                    if !a.value.split_whitespace().any(|q| q == p) {
                        a.value = format!("{} {p}", a.value).trim().to_string();
                    }
                }
            }
            None => {
                let mc = prefix_of(&self.attrs, MC).unwrap_or("mc").to_string();
                if prefix_of(&self.attrs, MC).is_none() {
                    self.attrs
                        .push(Attr::new(&format!("xmlns:{mc}"), XMLNS, MC));
                    self.attrs.last_mut().unwrap().local = mc.clone();
                }
                self.attrs
                    .push(Attr::new(&format!("{mc}:Ignorable"), MC, &wanted.join(" ")));
            }
        }
        local
    }
}

/// What a body uses, found by a first pass over it.
#[derive(Default)]
struct Uses {
    /// Relationship ids, in the order first met.
    rids: Vec<String>,
    styles: BTreeSet<String>,
    sections: usize,
    root: Vec<Attr>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Body,
    P,
    PPr,
    SectPr,
    Other,
}

fn tag(ns: &str, local: &[u8]) -> Tag {
    if ns != W {
        return Tag::Other;
    }
    match local {
        b"body" => Tag::Body,
        b"p" => Tag::P,
        b"pPr" => Tag::PPr,
        b"sectPr" => Tag::SectPr,
        _ => Tag::Other,
    }
}

/// Whether an attribute holds a relationship id of the part it is in.
fn is_rid(a: &Attr) -> bool {
    (a.ns == R || (a.ns == O && a.local == "relid")) && !a.value.is_empty()
}

fn scan(text: &str, part: &str, origin: &Origin) -> Result<Uses, Error> {
    let mut r = Reader::new(text, part, origin);
    let mut uses = Uses::default();
    let mut seen = HashSet::new();
    let mut stack: Vec<Tag> = vec![];
    let mut body_sect = false;
    loop {
        let event = r.event()?;
        let e = match &event {
            Event::Start(e) | Event::Empty(e) => e,
            Event::End(_) => {
                stack.pop();
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };
        let t = tag(r.element_ns(e), e.local_name().as_ref().as_bytes());
        let attrs = r.attrs(e)?;
        if stack.is_empty() {
            uses.root = attrs.clone();
        }
        if t == Tag::SectPr && matches!(stack.last(), Some(Tag::Body | Tag::PPr)) {
            uses.sections += 1;
            body_sect |= stack.last() == Some(&Tag::Body);
        }
        if r.element_ns(e) == W
            && matches!(e.local_name().as_ref(), "pStyle" | "rStyle" | "tblStyle")
            && let Some(a) = attrs.iter().find(|a| a.ns == W && a.local == "val")
        {
            uses.styles.insert(a.value.clone());
        }
        for a in attrs.iter().filter(|a| is_rid(a)) {
            if seen.insert(a.value.clone()) {
                uses.rids.push(a.value.clone());
            }
        }
        if matches!(event, Event::Start(_)) {
            stack.push(t);
        }
    }
    uses.sections += usize::from(!body_sect);
    Ok(uses)
}

/// How an appended document's content changes on its way into the output.
#[derive(Default)]
struct Rewrite {
    /// Old to new relationship ids (body only).
    rids: Option<HashMap<String, String>>,
    /// Old to new style ids.
    styles: HashMap<String, String>,
    /// Declarations each top-level element brings because the output root binds their prefix
    /// to another namespace.
    local: Vec<Attr>,
    /// This document's prefix for the main namespace.
    w: String,
    appended: bool,
}

impl Rewrite {
    /// Rewrites the attributes of an element; true when something changed.
    fn attrs(&self, ns: &str, local: &str, attrs: &mut [Attr]) -> bool {
        let mut changed = false;
        if ns == W
            && matches!(
                local,
                "pStyle"
                    | "rStyle"
                    | "tblStyle"
                    | "basedOn"
                    | "next"
                    | "link"
                    | "styleLink"
                    | "numStyleLink"
            )
            && let Some(a) = attrs.iter_mut().find(|a| a.ns == W && a.local == "val")
            && let Some(new) = self.styles.get(&a.value)
        {
            changed |= *new != a.value;
            a.value = new.clone();
        }
        if let Some(rids) = &self.rids {
            for a in attrs.iter_mut().filter(|a| is_rid(a)) {
                if let Some(new) = rids.get(&a.value) {
                    changed |= *new != a.value;
                    a.value = new.clone();
                }
            }
        }
        changed
    }
}

struct Role<'a> {
    base: bool,
    last: bool,
    root: &'a [Attr],
}

/// Where the body's section break goes in a held top-level paragraph.
#[derive(Clone, Copy)]
enum Mark {
    /// The paragraph has no properties: wrap the break in a new `w:pPr` here.
    Wrap(usize),
    /// Here, inside the paragraph's properties.
    Insert(usize),
    /// In place of the empty `<w:pPr/>` at this range.
    Expand(usize, usize),
}

fn put(buf: &mut Vec<u8>, event: &Event) {
    quick_xml::Writer::new(&mut *buf)
        .write_event(event.borrow())
        .expect("writing to memory");
}

/// Writes one input's body content to `out` (the base also writes what comes before its
/// body). Every input but the last puts its body's section break into its last paragraph, or
/// into a new one when the body ends with a table. Returns the base's text after its body, to
/// be written at the very end.
fn body(
    text: &str,
    part: &str,
    origin: &Origin,
    rw: &Rewrite,
    role: &Role,
    out: &mut dyn Write,
) -> Result<Option<String>, Error> {
    let mut r = Reader::new(text, part, origin);
    let w = rw.w.as_str();
    let mut stack: Vec<Tag> = vec![];
    // The depth of the body's children once inside it.
    let mut blocks: Option<usize> = None;
    let mut buf: Vec<u8> = Vec::with_capacity(64 << 10);
    let mut held: Option<(Vec<u8>, Mark)> = None;
    let mut mark: Option<Mark> = None;
    let mut plain = true;
    let mut sect: Option<Vec<u8>> = None;
    let mut first_sect = rw.appended;
    loop {
        let event = r.event()?;
        let depth = stack.len();
        let Some(b) = blocks else {
            // Before the body: the base copies it, with the root's declarations merged.
            match &event {
                Event::Eof => break,
                Event::Start(e) | Event::Empty(e) => {
                    let empty = matches!(event, Event::Empty(_));
                    let t = tag(r.element_ns(e), e.local_name().as_ref().as_bytes());
                    if role.base && depth == 0 {
                        let s = start_tag(e.name().as_ref(), role.root, empty);
                        out.write_all(s.as_bytes()).map_err(io)?;
                    } else if role.base {
                        put(&mut buf, &event);
                    }
                    if !empty {
                        stack.push(t);
                        if t == Tag::Body {
                            blocks = Some(stack.len());
                        }
                    }
                }
                Event::End(_) => {
                    stack.pop();
                    if role.base {
                        put(&mut buf, &event);
                    }
                }
                _ if role.base => put(&mut buf, &event),
                _ => {}
            }
            out.write_all(&buf).map_err(io)?;
            buf.clear();
            continue;
        };
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                let t = tag(r.element_ns(e), e.local_name().as_ref().as_bytes());
                let parent = stack.last().copied();
                if depth == b {
                    if let Some((p, _)) = held.take() {
                        out.write_all(&p).map_err(io)?;
                    }
                    out.write_all(&buf).map_err(io)?;
                    buf.clear();
                    mark = None;
                    plain = true;
                    if t == Tag::SectPr {
                        let mut s = vec![];
                        element(&mut r, e, empty, rw, &mut first_sect, true, &mut s)?;
                        sect = Some(s);
                        continue;
                    }
                }
                let at = buf.len();
                if depth == b + 2 && parent == Some(Tag::PPr) {
                    if e.local_name().as_ref() == "pPrChange" && r.element_ns(e) == W {
                        mark = Some(Mark::Insert(at));
                    }
                    plain &= t != Tag::SectPr;
                }
                let whole = element(&mut r, e, empty, rw, &mut first_sect, depth == b, &mut buf)?;
                let top_p = depth == b + 1 && stack[b] == Tag::P;
                match t {
                    Tag::P if depth == b && !empty => mark = Some(Mark::Wrap(buf.len())),
                    Tag::PPr if top_p && empty => mark = Some(Mark::Expand(at, buf.len())),
                    Tag::PPr if top_p => mark = None,
                    _ => {}
                }
                if !empty && !whole {
                    stack.push(t);
                }
            }
            Event::End(e) => {
                let t = stack.pop().unwrap_or(Tag::Other);
                let depth = stack.len();
                if t == Tag::Body {
                    let s = sect
                        .take()
                        .unwrap_or_else(|| format!("<{w}:sectPr/>").into_bytes());
                    match held.take() {
                        Some((p, _)) if role.last => out.write_all(&[p, buf, s].concat()),
                        None if role.last => out.write_all(&[buf, s].concat()),
                        Some((p, m)) => out.write_all(&[splice(p, m, &s, w), buf].concat()),
                        None => {
                            let p = [format!("<{w}:p><{w}:pPr>").into_bytes(), s];
                            let close = format!("</{w}:pPr></{w}:p>").into_bytes();
                            out.write_all(&[buf, p.concat(), close].concat())
                        }
                    }
                    .map_err(io)?;
                    let end = text.rfind(&format!("</{}", e.name().as_ref()));
                    return Ok(role
                        .base
                        .then(|| text[end.unwrap_or(text.len())..].to_string()));
                }
                if t == Tag::PPr
                    && depth == b + 1
                    && stack[b] == Tag::P
                    && !matches!(mark, Some(Mark::Insert(_)))
                {
                    mark = Some(Mark::Insert(buf.len()));
                }
                put(&mut buf, &event);
                if depth == b {
                    match mark {
                        Some(m) if t == Tag::P && plain => {
                            held = Some((std::mem::take(&mut buf), m))
                        }
                        _ => {
                            out.write_all(&buf).map_err(io)?;
                            buf.clear();
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => put(&mut buf, &event),
        }
    }
    Err(origin.bad(&format!("part {part} has no body")))
}

/// A held paragraph with the section break `s` put where `m` says.
fn splice(mut p: Vec<u8>, m: Mark, s: &[u8], w: &str) -> Vec<u8> {
    let wrapped = || {
        [
            format!("<{w}:pPr>").as_bytes(),
            s,
            format!("</{w}:pPr>").as_bytes(),
        ]
        .concat()
    };
    match m {
        Mark::Insert(at) => drop(p.splice(at..at, s.iter().copied())),
        Mark::Wrap(at) => drop(p.splice(at..at, wrapped())),
        Mark::Expand(from, to) => drop(p.splice(from..to, wrapped())),
    }
    p
}

/// Writes an element's start tag with the rewrite applied (a top-level one also declares the
/// prefixes the output root can't). A section break is written whole, and the first one of an
/// appended document starts a new page; returns true then.
fn element<'a>(
    r: &mut Reader<'a>,
    e: &BytesStart<'a>,
    empty: bool,
    rw: &Rewrite,
    first_sect: &mut bool,
    top: bool,
    buf: &mut Vec<u8>,
) -> Result<bool, Error> {
    let sect = tag(r.element_ns(e), e.local_name().as_ref().as_bytes()) == Tag::SectPr;
    let first = sect && std::mem::take(first_sect);
    start(r, e, empty, rw, top, false, buf)?;
    if !sect || empty {
        return Ok(false);
    }
    let depth = r.depth();
    loop {
        let event = r.event()?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                let new_page = first && r.depth() == depth + usize::from(!empty);
                start(r, e, empty, rw, false, new_page, buf)?;
            }
            Event::End(_) if r.depth() < depth => {
                put(buf, &event);
                return Ok(true);
            }
            Event::Eof => return Ok(true),
            _ => put(buf, &event),
        }
    }
}

/// One start tag, rewritten when the rewrite changes it; with `new_page`, a `w:type` that
/// would continue the page becomes `nextPage`.
fn start<'a>(
    r: &Reader<'a>,
    e: &BytesStart<'a>,
    empty: bool,
    rw: &Rewrite,
    top: bool,
    new_page: bool,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    let event = if empty {
        Event::Empty(e.borrow())
    } else {
        Event::Start(e.borrow())
    };
    if !rw.appended {
        put(buf, &event);
        return Ok(());
    }
    let mut attrs = r.attrs(e)?;
    let mut changed = rw.attrs(r.element_ns(e), e.local_name().as_ref(), &mut attrs);
    if top && !rw.local.is_empty() {
        attrs.extend(rw.local.iter().cloned());
        changed = true;
    }
    if new_page
        && r.element_ns(e) == W
        && e.local_name().as_ref() == "type"
        && let Some(v) = attrs.iter_mut().find(|a| a.ns == W && a.local == "val")
        && matches!(v.value.as_str(), "continuous" | "nextColumn")
    {
        v.value = "nextPage".into();
        changed = true;
    }
    if changed {
        buf.extend(start_tag(e.name().as_ref(), &attrs, empty).as_bytes());
    } else {
        put(buf, &event);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_merge_into_the_root_or_stay_local() {
        let decl = |p: &str, uri: &str| {
            let mut a = Attr::new(&format!("xmlns:{p}"), XMLNS, uri);
            a.local = p.into();
            a
        };
        let mut out = Prefixes::of(&[
            decl("w", W),
            decl("mc", MC),
            Attr::new("mc:Ignorable", MC, "w14"),
            decl("x", "urn:x"),
        ]);
        let local = out.merge(&[
            decl("w", W),
            decl("w15", "urn:w15"),
            decl("x", "urn:other"),
            decl("mc", MC),
            Attr::new("mc:Ignorable", MC, "w14 w15 x"),
        ]);
        assert_eq!(local, [decl("x", "urn:other")]);
        assert!(out.attrs.contains(&decl("w15", "urn:w15")));
        let ig = out.attrs.iter().find(|a| a.local == "Ignorable").unwrap();
        assert_eq!(ig.value, "w14 w15");
    }

    #[test]
    fn declarations_keep_their_line_break() {
        assert_eq!(
            declaration("<?xml version=\"1.0\"?>\r\n<a/>"),
            "<?xml version=\"1.0\"?>\r\n"
        );
        assert_eq!(declaration("<a/>"), "");
    }
}
