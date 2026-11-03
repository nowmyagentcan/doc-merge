//! Word: the bodies joined in order, every appended document starting on a new page with its
//! own sections, its styles mapped or copied (`styles`), its lists copied (`numbering`), its
//! notes and comments appended (`notes`), and the parts its content points to copied with new
//! relationship ids.

mod notes;
mod numbering;
mod styles;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;

use quick_xml::events::{BytesStart, Event};

use crate::error::{Error, Warning, Warnings};
use crate::package::{MAX_PARSED, MAX_STREAMED, Package, RELS_TYPE, Rel, rels_name, resolve};
use crate::writer::{Body, Plan, R, relative, rels_xml};
use crate::xml::{self, Attr, MC, Node, Reader, XMLNS, start_tag};
use crate::{Origin, Rename, StyleMode};
use notes::{KINDS, Notes};
use numbering::Lists;
use styles::{Sheet, Source, Theme};

pub const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const W15: &str = "http://schemas.microsoft.com/office/word/2012/wordml";
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
    let large = KINDS.iter().any(|k| k.0 == kind);
    let text = pkg.text(&part, if large { MAX_STREAMED } else { MAX_PARSED })?;
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
    let base_sheet = Sheet::new(base_styles.as_ref().map(|h| &h.tree));
    let base_defaults = base_sheet.defaults.clone();
    let base_p = base_sheet.default_of("paragraph").map(String::from);
    let base_table = base_sheet.default_of("table").map(String::from);
    let base_theme = theme(&mut inputs[0], &main, &base_rels)?;
    let mut out_styles = styles::Output::new(base_sheet);
    let base_numbering = held(&mut inputs[0], &main, &base_rels, "numbering")?;
    let mut num_root = Prefixes::of(base_numbering.as_ref().map_or(&[], |h| &h.tree.attrs));
    let num_part = base_numbering
        .as_ref()
        .map_or("word/numbering.xml", |h| &h.part)
        .to_string();
    let mut num_rels = Rels::new(inputs[0].rels_of(&num_part)?);
    let num_rels_before = num_rels.list.len();
    let mut out_lists =
        numbering::Output::new(Lists::new(base_numbering.as_ref().map(|h| &h.tree)));
    let mut notes = vec![];
    for kind in KINDS {
        notes.push(Notes::open(&mut inputs[0], &main, &base_rels, kind)?);
    }
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
                uses.nums.extend(found.nums);
                stories.push((id.clone(), target, text));
            }
        }
        let mut doc_notes = vec![];
        for (kind, _) in KINDS {
            let h = held(&mut inputs[i], &part, &own, kind)?;
            if let Some(h) = &h {
                let found = scan(&h.text, &h.part, &origin)?;
                uses.styles.extend(found.styles);
                uses.nums.extend(found.nums);
            }
            doc_notes.push(h);
        }
        let doc_styles = held(&mut inputs[i], &part, &own, "styles")?;
        let mut sheet = Sheet::new(doc_styles.as_ref().map(|h| &h.tree));
        let keep = mode == StyleMode::Keep;
        if keep && sheet.defaults_differ(base_defaults.as_ref()) {
            sheet.fold_defaults(base_defaults.as_ref());
        }
        let doc_theme = theme(&mut inputs[i], &part, &own)?
            .filter(|t| keep && base_theme.as_ref().is_some_and(|b| t.differs(b)));
        if doc_theme.is_some() {
            warnings.push(Warning::theme_differs(i + 1, &origin.name));
        }
        let doc_numbering = held(&mut inputs[i], &part, &own, "numbering")?;
        let lists = Lists::new(doc_numbering.as_ref().map(|h| &h.tree));
        let source = Source {
            sheet: &sheet,
            mode,
            position: i + 1,
            lists: &|n| lists.form(n),
            output_lists: &|n| out_lists.lists.form(n),
            themes: doc_theme.as_ref().zip(base_theme.as_ref()),
        };
        let mut seeds = uses.styles.clone();
        seeds.extend(
            ["paragraph", "table"]
                .iter()
                .filter_map(|k| sheet.default_of(k))
                .map(String::from),
        );
        seeds.extend(uses.nums.iter().flat_map(|n| lists.links(n)));
        let mapped = out_styles.map(&source, &sheet.closure(&seeds, &|n| lists.links(n)))?;
        renames.extend(mapped.renames.iter().map(|(from, to)| Rename {
            input: i + 1,
            kind: "style",
            from: from.clone(),
            to: to.clone(),
        }));
        let bare = |kind: &str, output: &Option<String>| {
            let new = mapped.ids.get(sheet.default_of(kind)?)?;
            (Some(new) != output.as_ref()).then(|| new.clone())
        };
        // Lists reached only through a style the output already has stay the output's.
        for s in &out_styles.styles[mapped.copied.clone()] {
            s.walk(&mut |n| {
                if n.is(W, "numId")
                    && let Some(v) = n.attr(W, "val")
                {
                    uses.nums.insert(v.to_string());
                }
            });
        }
        let num_source = doc_numbering.as_ref().map_or("", |h| h.part.as_str());
        let num_own = inputs[i].rels_of(num_source)?;
        let mut bullet = |old: &str| {
            let Some(r) = num_own.iter().find(|r| r.id == old) else {
                let why = format!("{num_source} points to a missing relationship {old}");
                return Err(origin.bad(&why));
            };
            let new = copy_rel(
                &mut plan,
                inputs,
                i,
                num_source,
                r,
                &num_part,
                &mut num_rels,
            )?;
            Ok(new.0)
        };
        let copied = out_lists.copy(&lists, &uses.nums, i + 1, &mut bullet)?;
        let mut rw = Rewrite {
            rids: Some(HashMap::new()),
            nums: copied.nums,
            notes: Default::default(),
            bare_p: bare("paragraph", &base_p),
            bare_table: bare("table", &base_table),
            theme: doc_theme,
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
        let local = num_root.merge(doc_numbering.as_ref().map_or(&[], |h| &h.tree.attrs));
        let added = &mut out_lists.lists;
        for s in &mut added.abstracts[copied.abstracts] {
            s.walk_mut(&mut |n| {
                rw.attrs(&n.ns, &n.local, &mut n.attrs);
            });
            s.attrs.extend(local.iter().cloned());
        }
        for s in added.bullets[copied.bullets]
            .iter_mut()
            .chain(&mut added.nums[copied.added_nums])
        {
            s.attrs.extend(local.iter().cloned());
        }
        for (k, h) in doc_notes.iter().enumerate() {
            if let Some(h) = h {
                rw.notes[k] = notes[k].add(&mut plan, inputs, i, h, &mut rw)?;
            }
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
    let origin = inputs[0].origin.clone();
    let parts = Parts {
        main: &main,
        kind: "styles",
    };
    let inserts: [(&[&str], &str); 1] = [(&[], &written(out_styles.added()))];
    let base = base_styles.as_ref();
    registry(
        &mut plan,
        &mut rels,
        &parts,
        base,
        &style_root,
        &inserts,
        &origin,
    )?;
    let [bullets, abstracts, nums] = out_lists.added().map(written);
    let inserts: [(&[&str], &str); 3] = [
        (&["abstractNum", "num", "numIdMacAtCleanup"], &bullets),
        (&["num", "numIdMacAtCleanup"], &abstracts),
        (&["numIdMacAtCleanup"], &nums),
    ];
    let parts = Parts {
        main: &main,
        kind: "numbering",
    };
    let base = base_numbering.as_ref();
    registry(
        &mut plan, &mut rels, &parts, base, &num_root, &inserts, &origin,
    )?;
    if num_rels.list.len() > num_rels_before {
        rels_part(&mut plan, &inputs[0], &num_part, &num_rels);
    }
    for n in &notes {
        n.finish(&mut plan, &mut rels, &inputs[0], &main, &origin)?;
    }
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
    rels_part(&mut plan, &inputs[0], &main, &rels);
    Ok(Merged {
        plan,
        sections,
        renames,
    })
}

/// Elements as text, one after another.
fn written(nodes: &[Node]) -> String {
    let mut s = String::new();
    for n in nodes {
        n.write(&mut s);
    }
    s
}

/// Writes the relationships of an output part, replacing the base's when it has them.
fn rels_part(plan: &mut Plan, base: &Package, part: &str, rels: &Rels) {
    let name = rels_name(part);
    if base.has(&name) {
        plan.replace(&name, Body::Data(rels_xml(&rels.list)));
    } else {
        plan.type_part(&name, RELS_TYPE, true);
        plan.add(name, Body::Data(rels_xml(&rels.list)), None);
    }
}

/// A document's theme, read with its `w:themeFontLang`.
fn theme(pkg: &mut Package, main: &str, rels: &[Rel]) -> Result<Option<Theme>, Error> {
    let settings = held(pkg, main, rels, "settings")?;
    let lang = settings
        .as_ref()
        .and_then(|h| h.tree.child(W, "themeFontLang"));
    Ok(held(pkg, main, rels, "theme")?.map(|h| Theme::new(&h.tree, lang)))
}

/// Which registry part is being written.
struct Parts<'a> {
    main: &'a str,
    /// The word its relationship type, file name, content type and root element share.
    kind: &'a str,
}

/// Writes a registry part with `inserts` (see [`extend`]) when any has content: the base's
/// part extended, or a new part when the base has none.
fn registry(
    plan: &mut Plan,
    rels: &mut Rels,
    parts: &Parts,
    base: Option<&Held>,
    root: &Prefixes,
    inserts: &[(&[&str], &str)],
    origin: &Origin,
) -> Result<(), Error> {
    if inserts.iter().all(|i| i.1.is_empty()) {
        return Ok(());
    }
    if let Some(h) = base {
        let bytes = extend(&h.text, &h.part, origin, &root.attrs, inserts)?;
        plan.replace(&h.part, Body::Data(bytes));
        return Ok(());
    }
    let w = prefix_of(&root.attrs, W).unwrap_or("w");
    let name = format!("{w}:{}", parts.kind);
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
    let mut bare = Bare::default();
    loop {
        let event = r.event()?;
        match &event {
            Event::Eof => return Ok(buf),
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                let place = Place::Inner;
                element(&mut r, e, empty, rw, &mut false, place, &mut bare, &mut buf)?;
            }
            Event::End(_) => {
                bare.end(rw, &mut buf);
                put(&mut buf, &event);
            }
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
    /// `w:numId`s, without 0, which means no list.
    nums: BTreeSet<String>,
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
        if r.element_ns(e) == W
            && e.local_name().as_ref() == "numId"
            && let Some(a) = attrs.iter().find(|a| a.ns == W && a.local == "val")
            && a.value != "0"
        {
            uses.nums.insert(a.value.clone());
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
    /// Old to new `w:numId`s.
    nums: HashMap<String, String>,
    /// Old to new note and comment ids, by [`KINDS`].
    notes: [HashMap<String, String>; 3],
    /// Set when theme links come off this document's content.
    theme: Option<Theme>,
    /// The style for paragraphs and tables with none, when not the output's default.
    bare_p: Option<String>,
    bare_table: Option<String>,
    /// Declarations each top-level element brings because the output root binds their prefix
    /// to another namespace.
    local: Vec<Attr>,
    /// This document's prefix for the main namespace.
    w: String,
    appended: bool,
}

impl Rewrite {
    /// Rewrites the attributes of an element; true when something changed.
    fn attrs(&self, ns: &str, local: &str, attrs: &mut Vec<Attr>) -> bool {
        let mut changed = self
            .theme
            .as_ref()
            .is_some_and(|t| t.resolve(ns, local, attrs));
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
        if ns == W
            && local == "numId"
            && let Some(a) = attrs.iter_mut().find(|a| a.ns == W && a.local == "val")
            && let Some(new) = self.nums.get(&a.value)
        {
            changed |= *new != a.value;
            a.value = new.clone();
        }
        let notes = match local {
            "footnoteReference" => Some(&self.notes[0]),
            "endnoteReference" => Some(&self.notes[1]),
            "commentReference" | "commentRangeStart" | "commentRangeEnd" => Some(&self.notes[2]),
            _ => None,
        };
        if ns == W
            && let Some(map) = notes
            && let Some(a) = attrs.iter_mut().find(|a| a.ns == W && a.local == "id")
            && let Some(new) = map.get(&a.value)
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
    let mut bare = Bare::default();
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
                if depth == b && t == Tag::SectPr {
                    let mut s = vec![];
                    let top = Place::Top;
                    element(
                        &mut r,
                        e,
                        empty,
                        rw,
                        &mut first_sect,
                        top,
                        &mut bare,
                        &mut s,
                    )?;
                    sect = Some(s);
                    continue;
                }
                if depth == b {
                    if let Some((p, _)) = held.take() {
                        out.write_all(&p).map_err(io)?;
                    }
                    out.write_all(&buf).map_err(io)?;
                    buf.clear();
                    mark = None;
                    plain = true;
                    if t == Tag::P && empty {
                        let top = Place::Top;
                        element(
                            &mut r,
                            e,
                            false,
                            rw,
                            &mut first_sect,
                            top,
                            &mut bare,
                            &mut buf,
                        )?;
                        let mut m = Mark::Wrap(buf.len());
                        bare.end(rw, &mut buf);
                        if let Some(at) = bare.made.take() {
                            m = Mark::Insert(at);
                        }
                        buf.extend(format!("</{}>", e.name().as_ref()).as_bytes());
                        held = Some((std::mem::take(&mut buf), m));
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
                let place = if depth == b { Place::Top } else { Place::Inner };
                let whole = element(
                    &mut r,
                    e,
                    empty,
                    rw,
                    &mut first_sect,
                    place,
                    &mut bare,
                    &mut buf,
                )?;
                let top_p = depth == b + 1 && stack[b] == Tag::P;
                match t {
                    Tag::P if depth == b && !empty => mark = Some(Mark::Wrap(buf.len())),
                    Tag::PPr if top_p && empty => mark = Some(Mark::Expand(at, buf.len())),
                    Tag::PPr if top_p => mark = None,
                    _ => {}
                }
                if let Some(at) = bare.made.take()
                    && top_p
                {
                    mark = Some(Mark::Insert(at));
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
                bare.end(rw, &mut buf);
                if let Some(at) = bare.made.take()
                    && t == Tag::P
                    && depth == b
                {
                    mark = Some(Mark::Insert(at));
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

/// Where an element is, for what it needs besides the rewrite.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Inner,
    /// A child of the body: it declares the prefixes the output root can't.
    Top,
    /// A child of an appended document's first section break.
    FirstSection,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    P,
    Table,
}

/// Gives paragraphs and tables with no style their document's default one, when that isn't the
/// output's default.
#[derive(Default)]
struct Bare {
    /// A block just opened, and whether its properties have begun.
    open: Option<(Block, bool)>,
    /// Where the last `w:pPr` made here ends, for a section break to go in.
    made: Option<usize>,
}

impl Bare {
    fn names(block: Block) -> (&'static str, &'static str) {
        match block {
            Block::P => ("pPr", "pStyle"),
            Block::Table => ("tblPr", "tblStyle"),
        }
    }

    fn style(rw: &Rewrite, block: Block, buf: &mut Vec<u8>) {
        let w = &rw.w;
        let id = match block {
            Block::P => &rw.bare_p,
            Block::Table => &rw.bare_table,
        };
        let attrs = [Attr::new(
            &format!("{w}:val"),
            W,
            id.as_deref().unwrap_or(""),
        )];
        let name = format!("{w}:{}", Self::names(block).1);
        buf.extend(start_tag(&name, &attrs, true).as_bytes());
    }

    /// Writes whole properties holding just the style.
    fn make(&mut self, rw: &Rewrite, block: Block, buf: &mut Vec<u8>) {
        let (props, w) = (Self::names(block).0, &rw.w);
        buf.extend(format!("<{w}:{props}>").as_bytes());
        Self::style(rw, block, buf);
        self.made = Some(buf.len());
        buf.extend(format!("</{w}:{props}>").as_bytes());
    }

    /// Runs before an element's start is written; true when this wrote it in its place.
    fn before(&mut self, rw: &Rewrite, name: (&str, &str), empty: bool, buf: &mut Vec<u8>) -> bool {
        let Some((block, inside)) = self.open else {
            return false;
        };
        let (props, style) = Self::names(block);
        let is = |local: &str| name.0 == W && name.1 == local;
        if inside {
            if !is(style) {
                Self::style(rw, block, buf);
            }
            self.open = None;
        } else if is(props) && empty {
            self.make(rw, block, buf);
            self.open = None;
            return true;
        } else if is(props) {
            self.open = Some((block, true));
        } else if block == Block::P {
            self.make(rw, block, buf);
            self.open = None;
        }
        false
    }

    /// Runs before an element's end is written.
    fn end(&mut self, rw: &Rewrite, buf: &mut Vec<u8>) {
        match self.open.take() {
            Some((block, true)) => Self::style(rw, block, buf),
            Some((Block::P, false)) => self.make(rw, Block::P, buf),
            _ => {}
        }
    }
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

/// Writes an element's start tag with the rewrite applied. A section break is written whole,
/// and the first one of an appended document starts a new page. An appended content control
/// loses its data binding, or Word could refill it from the base's data. Returns true when the
/// element's content has been dealt with too.
#[allow(clippy::too_many_arguments)]
fn element<'a>(
    r: &mut Reader<'a>,
    e: &BytesStart<'a>,
    empty: bool,
    rw: &Rewrite,
    first_sect: &mut bool,
    place: Place,
    bare: &mut Bare,
    buf: &mut Vec<u8>,
) -> Result<bool, Error> {
    let ns = r.element_ns(e);
    if rw.appended && (ns == W || ns == W15) && e.local_name().as_ref() == "dataBinding" {
        let depth = r.depth();
        while !empty && r.depth() >= depth {
            r.event()?;
        }
        return Ok(true);
    }
    let sect = tag(ns, e.local_name().as_ref().as_bytes()) == Tag::SectPr;
    let first = sect && std::mem::take(first_sect);
    start(r, e, empty, rw, place, bare, buf)?;
    if !sect || empty {
        return Ok(false);
    }
    let depth = r.depth();
    loop {
        let event = r.event()?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                let child = first && r.depth() == depth + usize::from(!empty);
                let place = if child {
                    Place::FirstSection
                } else {
                    Place::Inner
                };
                start(r, e, empty, rw, place, bare, buf)?;
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

/// One start tag, rebuilt only when the rewrite changes it.
fn start<'a>(
    r: &Reader<'a>,
    e: &BytesStart<'a>,
    empty: bool,
    rw: &Rewrite,
    place: Place,
    bare: &mut Bare,
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
    let (ns, local) = (r.element_ns(e), e.local_name());
    let local: &str = local.as_ref();
    if bare.before(rw, (ns, local), empty, buf) {
        return Ok(());
    }
    let mut attrs = r.attrs(e)?;
    let mut changed = rw.attrs(ns, local, &mut attrs);
    if place == Place::Top && !rw.local.is_empty() {
        attrs.extend(rw.local.iter().cloned());
        changed = true;
    }
    if place == Place::FirstSection
        && ns == W
        && local == "type"
        && let Some(v) = attrs.iter_mut().find(|a| a.ns == W && a.local == "val")
        && matches!(v.value.as_str(), "continuous" | "nextColumn")
    {
        v.value = "nextPage".into();
        changed = true;
    }
    let block = match (ns == W, local) {
        (true, "p") if rw.bare_p.is_some() => Some(Block::P),
        (true, "tbl") if rw.bare_table.is_some() => Some(Block::Table),
        _ => None,
    };
    let opened = empty && block == Some(Block::P);
    if changed || opened {
        buf.extend(start_tag(e.name().as_ref(), &attrs, empty && !opened).as_bytes());
    } else {
        put(buf, &event);
    }
    if opened {
        bare.make(rw, Block::P, buf);
        buf.extend(format!("</{}>", e.name().as_ref()).as_bytes());
    } else if !empty && let Some(b) = block {
        bare.open = Some((b, false));
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

    fn rewritten(body: &str, rw: &Rewrite) -> String {
        let text = format!(r#"<w:hdr xmlns:w="{W}">{body}</w:hdr>"#);
        let bytes = story(&text, "word/header1.xml", &Origin::test("x.docx"), rw).unwrap();
        let out = String::from_utf8(bytes).unwrap();
        let inner = out.split_once('>').unwrap().1;
        inner.strip_suffix("</w:hdr>").unwrap().to_string()
    }

    #[test]
    fn bare_paragraphs_and_tables_take_the_default_style() {
        let rw = Rewrite {
            w: "w".into(),
            appended: true,
            bare_p: Some("Normal_2".into()),
            bare_table: Some("Plain_2".into()),
            ..Rewrite::default()
        };
        let style = r#"<w:pStyle w:val="Normal_2"/>"#;
        for (input, want) in [
            ("<w:p/>", format!("<w:p><w:pPr>{style}</w:pPr></w:p>")),
            ("<w:p></w:p>", format!("<w:p><w:pPr>{style}</w:pPr></w:p>")),
            (
                "<w:p><w:r/></w:p>",
                format!("<w:p><w:pPr>{style}</w:pPr><w:r/></w:p>"),
            ),
            (
                "<w:p><w:pPr/></w:p>",
                format!("<w:p><w:pPr>{style}</w:pPr></w:p>"),
            ),
            (
                "<w:p><w:pPr></w:pPr></w:p>",
                format!("<w:p><w:pPr>{style}</w:pPr></w:p>"),
            ),
            (
                r#"<w:p><w:pPr><w:jc w:val="left"/></w:pPr></w:p>"#,
                format!(r#"<w:p><w:pPr>{style}<w:jc w:val="left"/></w:pPr></w:p>"#),
            ),
            (
                r#"<w:p><w:pPr><w:pStyle w:val="X"/></w:pPr></w:p>"#,
                r#"<w:p><w:pPr><w:pStyle w:val="X"/></w:pPr></w:p>"#.to_string(),
            ),
            (
                "<w:tbl><w:tblPr/><w:tblGrid/></w:tbl>",
                r#"<w:tbl><w:tblPr><w:tblStyle w:val="Plain_2"/></w:tblPr><w:tblGrid/></w:tbl>"#
                    .to_string(),
            ),
        ] {
            assert_eq!(rewritten(input, &rw), want, "{input}");
        }
    }

    fn joined(first: &str, second: &str) -> String {
        let doc =
            |b: &str| format!(r#"<w:document xmlns:w="{W}"><w:body>{b}</w:body></w:document>"#);
        let origin = Origin::test("x.docx");
        let plain = Rewrite {
            w: "w".into(),
            ..Rewrite::default()
        };
        let mut out: Vec<u8> = vec![];
        let root = [Attr::new("xmlns:w", XMLNS, W)];
        let role = |base, last| Role {
            base,
            last,
            root: &root,
        };
        let tail = body(
            &doc(first),
            "d",
            &origin,
            &plain,
            &role(true, false),
            &mut out,
        );
        let tail = tail.unwrap().unwrap();
        let appended = Rewrite {
            appended: true,
            w: "w".into(),
            ..Rewrite::default()
        };
        let second = doc(second);
        body(
            &second,
            "d",
            &origin,
            &appended,
            &role(false, true),
            &mut out,
        )
        .unwrap();
        out.extend(tail.as_bytes());
        let text = String::from_utf8(out).unwrap();
        let inner = text.split_once("<w:body>").unwrap().1;
        inner
            .strip_suffix("</w:body></w:document>")
            .unwrap()
            .to_string()
    }

    #[test]
    fn the_body_section_break_lands_in_the_last_paragraph() {
        let sect = r#"<w:sectPr><w:pgSz w:w="1"/></w:sectPr>"#;
        let carried = |p: &str| joined(&format!("{p}{sect}"), "<w:p/>");
        assert_eq!(
            carried("<w:p><w:r/></w:p>"),
            format!("<w:p><w:pPr>{sect}</w:pPr><w:r/></w:p><w:p></w:p><w:sectPr/>")
        );
        assert_eq!(
            carried("<w:p/>"),
            format!("<w:p><w:pPr>{sect}</w:pPr></w:p><w:p></w:p><w:sectPr/>")
        );
        assert_eq!(
            carried("<w:p><w:pPr/></w:p>"),
            format!("<w:p><w:pPr>{sect}</w:pPr></w:p><w:p></w:p><w:sectPr/>")
        );
        assert_eq!(
            carried(r#"<w:p><w:pPr><w:rPr/><w:pPrChange w:id="1"/></w:pPr></w:p>"#),
            format!(
                r#"<w:p><w:pPr><w:rPr/>{sect}<w:pPrChange w:id="1"/></w:pPr></w:p><w:p></w:p><w:sectPr/>"#
            )
        );
        assert_eq!(
            carried("<w:tbl/>"),
            format!("<w:tbl/><w:p><w:pPr>{sect}</w:pPr></w:p><w:p></w:p><w:sectPr/>")
        );
        let own = format!("<w:p><w:pPr>{sect}</w:pPr></w:p>");
        assert_eq!(
            carried(&own),
            format!("{own}<w:p><w:pPr>{sect}</w:pPr></w:p><w:p></w:p><w:sectPr/>")
        );
    }

    #[test]
    fn an_appended_first_section_starts_on_a_new_page() {
        let sect = |t: &str| format!(r#"<w:sectPr><w:type w:val="{t}"/></w:sectPr>"#);
        for (from, to) in [
            ("continuous", "nextPage"),
            ("nextColumn", "nextPage"),
            ("oddPage", "oddPage"),
            ("evenPage", "evenPage"),
        ] {
            let out = joined("<w:p/>", &format!("<w:p/>{}", sect(from)));
            assert!(out.ends_with(&sect(to)), "{out}");
        }
        let later = format!(
            "<w:p><w:pPr>{}</w:pPr></w:p><w:p/>{}",
            sect("oddPage"),
            sect("continuous")
        );
        assert!(joined("<w:p/>", &later).ends_with(&sect("continuous")));
    }
}
