//! Word: the bodies joined in order, every appended document starting on a new page with its
//! own sections, its styles mapped or copied (`styles`), its lists copied (`numbering`), its
//! notes and comments appended (`notes`), and the parts its content points to copied with new
//! relationship ids.

mod headers;
mod ids;
mod notes;
mod numbering;
mod settings;
mod styles;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::rc::Rc;

use quick_xml::events::{BytesStart, Event};

use crate::error::{Error, Warning, Warnings};
use crate::package::{MAX_PARSED, MAX_STREAMED, Package, Rel, resolve};
use crate::writer::{Body, Plan, R, Rels, relative, rels_part};
use crate::xml::{self, Attr, Node, Prefixes, Reader, declaration, prefix_of, start_tag, written};
use crate::{Origin, Rename, StyleMode};
use headers::Slot;
use ids::{Defects, Field, Ids, Known, Local, Tally};
use notes::{KINDS, Notes, Sides};
use numbering::Lists;
use settings::{Fonts, Settings};
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
    let large = KINDS.iter().any(|k| k.0 == kind);
    let cap = if large { MAX_STREAMED } else { MAX_PARSED };
    held_by(pkg, main, rels, &format!("{REL}{kind}"), cap)
}

/// The part with relationship type `wanted`.
fn held_by(
    pkg: &mut Package,
    main: &str,
    rels: &[Rel],
    wanted: &str,
    cap: u64,
) -> Result<Option<Held>, Error> {
    let Some(r) = rels.iter().find(|r| r.kind == wanted && !r.external) else {
        return Ok(None);
    };
    let part = resolve(main, &r.target).and_then(|t| pkg.find(&t).map(|e| e.name.clone()));
    let Some(part) = part else {
        return Err(pkg
            .origin
            .bad(&format!("{main} points to a missing part {}", r.target)));
    };
    let text = pkg.text(&part, cap)?;
    let tree = xml::tree(&text, &part, &pkg.origin)?;
    Ok(Some(Held { part, text, tree }))
}

pub fn merge(
    inputs: &mut [Package],
    mode: StyleMode,
    warnings: &mut Warnings,
) -> Result<Merged, Error> {
    let mut m = Merger::new(inputs, mode, warnings)?;
    m.add_base()?;
    for i in 1..m.inputs.len() {
        m.append(i)?;
    }
    m.finish()
}

/// A header or footer an appended document uses: its relationship id, part and text.
type Story = (String, String, String);

/// The output as planned so far, with what appended documents are compared with.
struct Merger<'a> {
    inputs: &'a mut [Package],
    warnings: &'a mut Warnings,
    mode: StyleMode,
    plan: Plan,
    main: String,
    rels: Rels,
    settings: Vec<Settings>,
    fonts: Fonts,
    has_glossary: bool,
    /// Whether the output holds a table of contents.
    has_toc: bool,
    /// The first appended document with headings, which a table of contents would list.
    headings: Option<usize>,
    base_styles: Option<Held>,
    style_root: Prefixes,
    base_defaults: Option<Node>,
    base_p: Option<String>,
    base_table: Option<String>,
    base_theme: Option<Theme>,
    out_styles: styles::Output,
    base_numbering: Option<Held>,
    num_root: Prefixes,
    num_part: String,
    num_rels: Rels,
    num_rels_before: usize,
    out_lists: numbering::Output,
    ids: Rc<RefCell<Ids>>,
    /// Bookmark names in the output, in lower case.
    bookmarks: HashSet<String>,
    /// Paragraph ids of the main part.
    paras: HashSet<u32>,
    /// What the output's content holds, and the defects its inputs brought.
    tally: Rc<RefCell<Tally>>,
    defects: Defects,
    notes: Vec<Notes>,
    sides: Sides,
    root: Prefixes,
    docs: Vec<Rewrite>,
    sections: Vec<usize>,
    renames: Vec<Rename>,
}

impl<'a> Merger<'a> {
    fn new(
        inputs: &'a mut [Package],
        mode: StyleMode,
        warnings: &'a mut Warnings,
    ) -> Result<Self, Error> {
        let plan = Plan::base(&inputs[0], warnings)?;
        let main = inputs[0].main.clone();
        let base_rels = inputs[0].rels_of(&main)?;
        let mut settings = vec![];
        for pkg in inputs.iter_mut() {
            let part = pkg.main.clone();
            let rels = pkg.rels_of(&part)?;
            let held = held(pkg, &part, &rels, "settings")?;
            settings.push(Settings::new(held.as_ref().map(|h| &h.tree)));
        }
        let base_styles = held(&mut inputs[0], &main, &base_rels, "styles")?;
        let base_sheet = Sheet::new(base_styles.as_ref().map(|h| &h.tree));
        let base_numbering = held(&mut inputs[0], &main, &base_rels, "numbering")?;
        let num_part = base_numbering
            .as_ref()
            .map_or("word/numbering.xml", |h| &h.part)
            .to_string();
        let num_rels = Rels::new(inputs[0].rels_of(&num_part)?);
        let mut notes = vec![];
        for kind in KINDS {
            notes.push(Notes::open(&mut inputs[0], &main, &base_rels, kind)?);
        }
        let sides = Sides::open(&mut inputs[0], &main, &base_rels)?;
        Ok(Self {
            plan,
            rels: Rels::new(base_rels.clone()),
            settings,
            fonts: Fonts::open(&mut inputs[0], &main, &base_rels)?,
            has_glossary: base_rels
                .iter()
                .any(|r| r.kind == format!("{REL}glossaryDocument")),
            has_toc: false,
            headings: None,
            style_root: Prefixes::of(base_styles.as_ref().map_or(&[], |h| &h.tree.attrs)),
            base_defaults: base_sheet.defaults.clone(),
            base_p: base_sheet.default_of("paragraph").map(String::from),
            base_table: base_sheet.default_of("table").map(String::from),
            base_theme: theme(&mut inputs[0], &main, &base_rels)?,
            out_styles: styles::Output::new(base_sheet),
            num_root: Prefixes::of(base_numbering.as_ref().map_or(&[], |h| &h.tree.attrs)),
            num_rels_before: num_rels.list.len(),
            num_rels,
            num_part,
            out_lists: numbering::Output::new(Lists::new(base_numbering.as_ref().map(|h| &h.tree))),
            base_styles,
            base_numbering,
            ids: Rc::default(),
            bookmarks: HashSet::new(),
            paras: HashSet::new(),
            tally: Rc::default(),
            defects: Defects::default(),
            notes,
            sides,
            root: Prefixes::default(),
            docs: vec![],
            sections: vec![],
            renames: vec![],
            main,
            inputs,
            warnings,
            mode,
        })
    }

    /// Whether input `i`'s even pages show its default headers, though the output has even
    /// and odd headers on.
    fn needs_twins(&self, i: usize) -> bool {
        !self.settings[i].even && self.settings.iter().any(|s| s.even)
    }

    /// What the output's content can point to so far.
    fn known(&self) -> Known {
        let [footnotes, endnotes, comments] = [0, 1, 2].map(|k| self.notes[k].ids());
        let styles = self.out_styles.ids();
        [
            styles,
            self.out_lists.lists.ids(),
            footnotes,
            endnotes,
            comments,
        ]
    }

    fn count_defects(&mut self, tally: &Tally, known: &Known) {
        for (sum, n) in self.defects.iter_mut().zip(tally.defects(known)) {
            *sum += n;
        }
    }

    fn add_base(&mut self) -> Result<(), Error> {
        let origin = self.inputs[0].origin.clone();
        let main = self.main.clone();
        let twins = self.needs_twins(0);
        let text = self.inputs[0].text(&main, MAX_STREAMED)?;
        let mut uses = scan(&text, &main, &origin, twins)?;
        drop(text);
        self.sections.push(uses.sections);
        self.paras = uses.paras.iter().copied().collect();
        let own = self.inputs[0].rels_of(&main)?;
        self.stories(0, &own, &mut uses)?;
        for n in &mut self.notes {
            uses.add(n.scan_base(&origin)?);
        }
        self.has_toc = uses.toc;
        self.ids = Rc::new(RefCell::new(Ids::base(&uses.tally)));
        self.bookmarks = uses.names.iter().map(|n| n.to_lowercase()).collect();
        self.root = Prefixes::of(&uses.root);
        let known = self.known();
        self.count_defects(&uses.tally, &known);
        self.tally.borrow_mut().add(std::mem::take(&mut uses.tally));
        let mut rw = Rewrite {
            w: prefix_of(&uses.root, W).unwrap_or("w").to_string(),
            r: prefix_of(&uses.root, R).map(String::from),
            ids: self.ids.clone(),
            tally: self.tally.clone(),
            ..Rewrite::default()
        };
        if twins {
            self.add_twins(0, &own, &uses, &mut rw)?;
        }
        self.docs.push(rw);
        Ok(())
    }

    /// The headers and footers input `i`'s body points to, with what they use added to `uses`.
    fn stories(&mut self, i: usize, own: &[Rel], uses: &mut Uses) -> Result<Vec<Story>, Error> {
        let origin = self.inputs[i].origin.clone();
        let part = self.inputs[i].main.clone();
        let mut stories = vec![];
        for id in &uses.rids.clone() {
            let Some(r) = own.iter().find(|r| &r.id == id) else {
                if i == 0 {
                    continue;
                }
                return Err(origin.bad(&format!("{part} points to a missing relationship {id}")));
            };
            if r.external || ![format!("{REL}header"), format!("{REL}footer")].contains(&r.kind) {
                continue;
            }
            let Some(target) = resolve(&part, &r.target) else {
                continue;
            };
            if self.inputs[i].has(&target) {
                let text = self.inputs[i].text(&target, MAX_PARSED)?;
                uses.add(scan(&text, &target, &origin, false)?);
                stories.push((id.clone(), target, text));
            }
        }
        Ok(stories)
    }

    fn append(&mut self, i: usize) -> Result<(), Error> {
        let part = self.inputs[i].main.clone();
        let origin = self.inputs[i].origin.clone();
        let twins = self.needs_twins(i);
        let text = self.inputs[i].text(&part, MAX_STREAMED)?;
        let mut uses = scan(&text, &part, &origin, twins)?;
        drop(text);
        self.sections.push(uses.sections);
        let body_paras = std::mem::take(&mut uses.paras);
        let own = self.inputs[i].rels_of(&part)?;
        let stories = self.stories(i, &own, &mut uses)?;
        let mut doc_notes = vec![];
        for (kind, _) in KINDS {
            let h = held(&mut self.inputs[i], &part, &own, kind)?;
            if let Some(h) = &h {
                uses.add(scan(&h.text, &h.part, &origin, false)?);
            }
            doc_notes.push(h);
        }
        let doc_styles = held(&mut self.inputs[i], &part, &own, "styles")?;
        let mut sheet = Sheet::new(doc_styles.as_ref().map(|h| &h.tree));
        let keep = self.mode == StyleMode::Keep;
        if keep && sheet.defaults_differ(self.base_defaults.as_ref()) {
            sheet.fold_defaults(self.base_defaults.as_ref());
        }
        let base_theme = self.base_theme.as_ref();
        let doc_theme = theme(&mut self.inputs[i], &part, &own)?
            .filter(|t| keep && base_theme.is_some_and(|b| t.differs(b)));
        if doc_theme.is_some() {
            self.warnings
                .push(Warning::theme_differs(i + 1, &origin.name));
        }
        let doc_numbering = held(&mut self.inputs[i], &part, &own, "numbering")?;
        let lists = Lists::new(doc_numbering.as_ref().map(|h| &h.tree));
        let out_lists = &self.out_lists;
        let source = Source {
            sheet: &sheet,
            mode: self.mode,
            position: i + 1,
            lists: &|n| lists.form(n),
            output_lists: &|n| out_lists.lists.form(n),
            themes: doc_theme.as_ref().zip(base_theme),
        };
        let mut seeds = uses.styles.clone();
        seeds.extend(
            ["paragraph", "table"]
                .iter()
                .filter_map(|k| sheet.default_of(k))
                .map(String::from),
        );
        seeds.extend(uses.nums.iter().flat_map(|n| lists.links(n)));
        let used = sheet.closure(&seeds, &|n| lists.links(n));
        let mapped = self.out_styles.map(&source, &used)?;
        self.renames
            .extend(mapped.renames.iter().map(|(from, to)| Rename {
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
        for s in &self.out_styles.styles[mapped.copied.clone()] {
            s.walk(&mut |n| {
                if n.is(W, "numId")
                    && let Some(v) = n.attr(W, "val")
                {
                    uses.nums.insert(v.to_string());
                }
            });
        }
        let num_source = doc_numbering.as_ref().map_or("", |h| h.part.as_str());
        let num_own = self.inputs[i].rels_of(num_source)?;
        let (plan, inputs) = (&mut self.plan, &mut *self.inputs);
        let (num_part, num_rels) = (&self.num_part, &mut self.num_rels);
        let mut bullet = |old: &str| {
            let Some(r) = num_own.iter().find(|r| r.id == old) else {
                let why = format!("{num_source} points to a missing relationship {old}");
                return Err(origin.bad(&why));
            };
            let new = copy_rel(plan, inputs, i, num_source, r, num_part, num_rels)?;
            Ok(new.0)
        };
        let copied = self
            .out_lists
            .copy(&lists, &uses.nums, i + 1, &mut bullet)?;
        let note_ids = |k: usize| {
            doc_notes[k]
                .as_ref()
                .map_or(HashSet::new(), notes::item_ids)
        };
        let known = [
            sheet.ids(),
            lists.ids(),
            note_ids(0),
            note_ids(1),
            note_ids(2),
        ];
        self.count_defects(&uses.tally, &known);
        uses.names
            .retain(|n| !n.eq_ignore_ascii_case(ids::HIDDEN_BOOKMARK));
        let (names, renamed) = ids::rename_bookmarks(&uses.names, i + 1, &mut self.bookmarks);
        self.renames
            .extend(renamed.into_iter().map(|(from, to)| Rename {
                input: i + 1,
                kind: "bookmark",
                from,
                to,
            }));
        self.has_toc |= uses.toc;
        if self.headings.is_none()
            && (uses.outline || uses.styles.iter().any(|s| sheet.outlined(s)))
        {
            self.headings = Some(i + 1);
        }
        let mut rw = Rewrite {
            nums: copied.nums,
            names,
            ids: self.ids.clone(),
            tally: self.tally.clone(),
            bare_p: bare("paragraph", &self.base_p),
            bare_table: bare("table", &self.base_table),
            theme: doc_theme,
            styles: mapped.ids,
            local: self.root.merge(&uses.root),
            w: prefix_of(&uses.root, W).unwrap_or("w").to_string(),
            r: prefix_of(&uses.root, R).map(String::from),
            appended: true,
            ..Rewrite::default()
        };
        let local = self
            .style_root
            .merge(doc_styles.as_ref().map_or(&[], |h| &h.tree.attrs));
        for s in &mut self.out_styles.styles[mapped.copied] {
            s.walk_mut(&mut |n| {
                rw.attrs(&n.ns, &n.local, &mut n.attrs);
            });
            s.attrs.extend(local.iter().cloned());
        }
        let local = self
            .num_root
            .merge(doc_numbering.as_ref().map_or(&[], |h| &h.tree.attrs));
        let added = &mut self.out_lists.lists;
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
        let mut comment_paras = HashMap::new();
        for (k, h) in doc_notes.iter().enumerate() {
            if let Some(h) = h {
                let (ids, paras) = self.notes[k].add(&mut self.plan, self.inputs, i, h, &mut rw)?;
                rw.notes[k] = ids;
                comment_paras = paras;
            }
        }
        self.sides.add(&mut self.inputs[i], &own, &comment_paras)?;
        self.apply_settings(i, &mut rw);
        self.document_parts(i, &own, &uses)?;
        self.copy_body_rels(i, &own, &uses, &stories, &mut rw)?;
        if twins {
            self.add_twins(i, &own, &uses, &mut rw)?;
        }
        self.fill_slots(&uses, &mut rw, twins);
        rw.paras = ids::new_values(i + 1, &body_paras, &mut self.paras);
        self.docs.push(rw);
        Ok(())
    }

    /// Copies what input `i`'s body points to, each header and footer with the rewrite applied.
    fn copy_body_rels(
        &mut self,
        i: usize,
        own: &[Rel],
        uses: &Uses,
        stories: &[Story],
        rw: &mut Rewrite,
    ) -> Result<(), Error> {
        let part = self.inputs[i].main.clone();
        let origin = self.inputs[i].origin.clone();
        let mut rids = HashMap::new();
        let mut done = HashSet::new();
        for id in &uses.rids {
            let Some(r) = own.iter().find(|r| &r.id == id) else {
                continue;
            };
            let (plan, inputs) = (&mut self.plan, &mut *self.inputs);
            let (new, copied) = copy_rel(plan, inputs, i, &part, r, &self.main, &mut self.rels)?;
            rids.insert(id.clone(), new);
            if let Some((_, source, text)) = stories.iter().find(|s| &s.0 == id)
                && let Some(copied) = copied
                && done.insert(copied.clone())
            {
                let body = story(text, source, &origin, rw)?;
                self.plan.replace(&copied, Body::Data(body));
            }
        }
        rw.rids = Some(rids);
        Ok(())
    }

    fn finish(mut self) -> Result<Merged, Error> {
        let origin = self.inputs[0].origin.clone();
        let main = self.main.clone();
        let parts = Parts {
            main: &main,
            kind: "styles",
            root: "styles",
        };
        let inserts: [(&[&str], &str); 1] = [(&[], &written(self.out_styles.added()))];
        let base = self.base_styles.as_ref();
        registry(
            &mut self.plan,
            &mut self.rels,
            &parts,
            base,
            &self.style_root,
            &inserts,
            &origin,
        )?;
        let [bullets, abstracts, nums] = self.out_lists.added().map(written);
        let inserts: [(&[&str], &str); 3] = [
            (&["abstractNum", "num", "numIdMacAtCleanup"], &bullets),
            (&["num", "numIdMacAtCleanup"], &abstracts),
            (&["numIdMacAtCleanup"], &nums),
        ];
        let parts = Parts {
            main: &main,
            kind: "numbering",
            root: "numbering",
        };
        let base = self.base_numbering.as_ref();
        registry(
            &mut self.plan,
            &mut self.rels,
            &parts,
            base,
            &self.num_root,
            &inserts,
            &origin,
        )?;
        if self.num_rels.list.len() > self.num_rels_before {
            rels_part(
                &mut self.plan,
                &self.inputs[0],
                &self.num_part,
                &self.num_rels,
            );
        }
        for n in &self.notes {
            n.finish(
                &mut self.plan,
                &mut self.rels,
                &self.inputs[0],
                &main,
                &origin,
            )?;
        }
        self.sides
            .finish(&mut self.plan, &mut self.rels, &main, &origin)?;
        self.finish_settings(&origin)?;
        if let Some(input) = self.headings.filter(|_| self.has_toc) {
            self.warnings.push(Warning::toc_needs_update(input));
        }
        let known = self.known();
        let (tally, defects) = (self.tally, self.defects);
        let root_attrs = self.root.attrs;
        let count = self.inputs.len();
        let docs = self.docs;
        self.plan.replace(
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
                ids::check(&tally.borrow().defects(&known), &defects)?;
                out.write_all(tail.as_bytes()).map_err(io)
            })),
        );
        rels_part(&mut self.plan, &self.inputs[0], &main, &self.rels);
        Ok(Merged {
            plan: self.plan,
            sections: self.sections,
            renames: self.renames,
        })
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
    /// The word its relationship type, file name and content type share.
    kind: &'a str,
    root: &'a str,
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
    let name = format!("{w}:{}", parts.root);
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
    let mut walk = Walk::default();
    loop {
        let event = r.event()?;
        match &event {
            Event::Eof => return Ok(buf),
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(event, Event::Empty(_));
                element(&mut r, e, empty, rw, &mut walk, Place::Inner, &mut buf)?;
            }
            Event::End(e) => {
                walk.bare.end(rw, &mut buf);
                walk.closing(e.local_name().as_ref(), rw, &mut buf);
                put(&mut buf, &event);
            }
            _ => put(&mut buf, &event),
        }
    }
}

fn io(e: std::io::Error) -> Error {
    Error::write_failed("the merged file", &e)
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
    let mut id = None;
    tree.walk(&mut |n| {
        if n.is(DSP, "dataModelExt") {
            id = n.attr("", "relId").map(String::from);
        }
    });
    let Some(r) = own
        .iter()
        .find(|r| Some(&r.id) == id.as_ref() && r.kind == DIAGRAM_DRAWING)
    else {
        return Ok(());
    };
    let (new, _) = copy_rel(plan, inputs, i, part, r, to, rels)?;
    tree.walk_mut(&mut |n| {
        if n.is(DSP, "dataModelExt") {
            n.set_attr("", "", "relId", &new);
        }
    });
    let mut out = declaration(&text);
    tree.write(&mut out);
    plan.replace(copied, Body::Data(out.into_bytes()));
    Ok(())
}

/// What a body uses, found by a first pass over it.
#[derive(Default)]
struct Uses {
    /// Relationship ids, in the order first met.
    rids: Vec<String>,
    styles: BTreeSet<String>,
    /// `w:numId`s, without 0, which means no list.
    nums: BTreeSet<String>,
    /// Bookmark names, in the order first met.
    names: Vec<String>,
    tally: Tally,
    /// `w14:paraId`s and `w14:textId`s of this part.
    paras: BTreeSet<u32>,
    sections: usize,
    root: Vec<Attr>,
    /// Relationship ids of the default header and footer references of every section.
    defaults: Vec<String>,
    /// Which references the first section has: (element name, type).
    first_slots: HashSet<(&'static str, String)>,
    toc: bool,
    /// Whether a paragraph sets its own outline level.
    outline: bool,
}

impl Uses {
    /// Adds what a header, footer or notes part of the same document uses.
    fn add(&mut self, other: Uses) {
        self.styles.extend(other.styles);
        self.nums.extend(other.nums);
        self.names.extend(other.names);
        self.tally.add(other.tally);
        self.toc |= other.toc;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Body,
    P,
    PPr,
    SectPr,
    Instr,
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
        b"instrText" => Tag::Instr,
        _ => Tag::Other,
    }
}

/// Whether an attribute holds a relationship id of the part it is in.
fn is_rid(a: &Attr) -> bool {
    (a.ns == R || (a.ns == O && a.local == "relid")) && !a.value.is_empty()
}

/// How many sections a document has.
pub fn sections(pkg: &mut Package) -> Result<usize, Error> {
    let text = pkg.text(&pkg.main.clone(), MAX_STREAMED)?;
    Ok(scan(&text, &pkg.main, &pkg.origin, false)?.sections)
}

/// What a part uses; `twins` leaves out its even header and footer references.
fn scan(text: &str, part: &str, origin: &Origin, twins: bool) -> Result<Uses, Error> {
    let mut r = Reader::new(text, part, origin);
    let mut uses = Uses::default();
    let mut seen = HashSet::new();
    let mut stack: Vec<Tag> = vec![];
    let mut body_sect = false;
    let mut instr: Option<String> = None;
    let is_toc = |s: &str| {
        let name = s.split_whitespace().next();
        name.is_some_and(|n| n.eq_ignore_ascii_case("TOC"))
    };
    loop {
        let event = r.event()?;
        let e = match &event {
            Event::Start(e) | Event::Empty(e) => e,
            Event::End(_) => {
                stack.pop();
                continue;
            }
            Event::Text(t) => {
                if stack.last() == Some(&Tag::Instr) {
                    instr.get_or_insert_default().push_str(&t.xml10_content());
                }
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
        if r.element_ns(e) == W
            && e.local_name().as_ref() == "bookmarkStart"
            && let Some(a) = attrs.iter().find(|a| a.ns == W && a.local == "name")
        {
            uses.names.push(a.value.clone());
        }
        uses.tally
            .see(r.element_ns(e), e.local_name().as_ref(), &attrs);
        uses.paras
            .extend(attrs.iter().filter_map(ids::paragraph_id));
        if r.element_ns(e) == W {
            match e.local_name().as_ref() {
                "fldChar" => uses.toc |= instr.take().is_some_and(|s| is_toc(&s)),
                "fldSimple" => {
                    let a = attrs.iter().find(|a| a.ns == W && a.local == "instr");
                    uses.toc |= a.is_some_and(|a| is_toc(&a.value));
                }
                "outlineLvl" => {
                    let a = attrs.iter().find(|a| a.ns == W && a.local == "val");
                    uses.outline |= a.is_some_and(|a| a.value != "9");
                }
                _ => {}
            }
        }
        let reference = headers::reference(r.element_ns(e), e.local_name().as_ref());
        if let Some(local) = reference.filter(|_| stack.last() == Some(&Tag::SectPr)) {
            let kind = attrs.iter().find(|a| a.ns == W && a.local == "type");
            let kind = kind.map_or("default", |a| a.value.as_str());
            if twins && kind == "even" {
                continue;
            }
            if uses.sections == 1 {
                uses.first_slots.insert((local, kind.to_string()));
            }
            if kind == "default" {
                let rid = attrs.iter().find(|a| is_rid(a));
                uses.defaults.extend(rid.map(|a| a.value.clone()));
            }
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
    /// The output's id pools, and what its content holds so far.
    ids: Rc<RefCell<Ids>>,
    tally: Rc<RefCell<Tally>>,
    /// Old (lower case) to new bookmark names.
    names: HashMap<String, String>,
    /// Old to new paragraph ids of the part being written.
    paras: HashMap<u32, u32>,
    /// Set while writing a copy of a part already in the output, which brings no bookmarks.
    copying: bool,
    /// Set when theme links come off this document's content.
    theme: Option<Theme>,
    /// The style for paragraphs and tables with none, when not the output's default.
    bare_p: Option<String>,
    bare_table: Option<String>,
    /// Declarations each top-level element brings because the output root binds their prefix
    /// to another namespace.
    local: Vec<Attr>,
    /// This document's prefix for the main namespace, and for relationships if it has one.
    w: String,
    r: Option<String>,
    appended: bool,
    /// Set when this document's even references go, each default one getting a twin: old
    /// relationship ids to the twins' new ones.
    twin: bool,
    twins: HashMap<String, String>,
    /// What the first section gets for the slots it leaves open.
    fill: Vec<Slot>,
    /// What every section with none gets as its `w:footnotePr` and `w:endnotePr`.
    note_format: [Option<String>; 2],
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
        if ns == W && !self.names.is_empty() {
            changed |= self.rename_bookmark(local, attrs);
        }
        if !self.paras.is_empty() {
            for a in attrs.iter_mut() {
                if let Some(new) = ids::paragraph_id(a).and_then(|v| self.paras.get(&v)) {
                    a.value = format!("{new:08X}");
                    changed = true;
                }
            }
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

impl Rewrite {
    /// Renames a bookmark, or a link or simple field pointing to one; true when it did.
    fn rename_bookmark(&self, local: &str, attrs: &mut [Attr]) -> bool {
        let key = match local {
            "bookmarkStart" => "name",
            "hyperlink" => "anchor",
            "fldSimple" => "instr",
            _ => return false,
        };
        let Some(a) = attrs.iter_mut().find(|a| a.ns == W && a.local == key) else {
            return false;
        };
        let new = if local == "fldSimple" {
            ids::rename_in_field(&a.value, &self.names)
        } else {
            self.names.get(&a.value.to_lowercase()).cloned()
        };
        let changed = new.is_some();
        a.value = new.unwrap_or_else(|| std::mem::take(&mut a.value));
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
    let mut walk = Walk {
        first_sect: rw.appended,
        ..Walk::default()
    };
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
                    element(&mut r, e, empty, rw, &mut walk, Place::Top, &mut s)?;
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
                        element(&mut r, e, false, rw, &mut walk, Place::Top, &mut buf)?;
                        let mut m = Mark::Wrap(buf.len());
                        walk.bare.end(rw, &mut buf);
                        if let Some(at) = walk.bare.made.take() {
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
                let whole = element(&mut r, e, empty, rw, &mut walk, place, &mut buf)?;
                let top_p = depth == b + 1 && stack[b] == Tag::P;
                match t {
                    Tag::P if depth == b && !empty => mark = Some(Mark::Wrap(buf.len())),
                    Tag::PPr if top_p && empty => mark = Some(Mark::Expand(at, buf.len())),
                    Tag::PPr if top_p => mark = None,
                    _ => {}
                }
                if let Some(at) = walk.bare.made.take()
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
                        .unwrap_or_else(|| headers::missing_section(rw, walk.first_sect));
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
                walk.bare.end(rw, &mut buf);
                if let Some(at) = walk.bare.made.take()
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
                walk.closing(e.local_name().as_ref(), rw, &mut buf);
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

/// What a walk over one part keeps between elements.
#[derive(Default)]
struct Walk {
    bare: Bare,
    /// Whether the next section break is an appended document's first.
    first_sect: bool,
    local: Local,
    field: Field,
}

impl Walk {
    /// Runs before an element's end is written.
    fn closing(&mut self, local: &str, rw: &Rewrite, buf: &mut Vec<u8>) {
        match local {
            "instrText" | "delInstrText" => self.field.close(buf.len()),
            "p" => self.field.settle(&rw.names, buf),
            _ => {}
        }
    }
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

/// Writes an element's start tag with the rewrite applied. A section break is written whole.
/// An appended content control loses its data binding, or Word could refill it from the base's
/// data. Returns true when the element's content has been dealt with too.
fn element<'a>(
    r: &mut Reader<'a>,
    e: &BytesStart<'a>,
    empty: bool,
    rw: &Rewrite,
    walk: &mut Walk,
    place: Place,
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
    let local = e.local_name();
    let local: &str = local.as_ref();
    if rw.appended
        && ns == W
        && matches!(local, "bookmarkStart" | "bookmarkEnd")
        && walk.local.leaves_out(local, &r.attrs(e)?, rw.copying)
    {
        let depth = r.depth();
        while !empty && r.depth() >= depth {
            r.event()?;
        }
        return Ok(true);
    }
    if tag(ns, local.as_bytes()) == Tag::SectPr {
        headers::section(r, e, empty, rw, walk, place, buf)?;
        return Ok(true);
    }
    start(r, e, empty, rw, place, walk, buf)?;
    Ok(false)
}

/// One start tag, rebuilt only when the rewrite changes it.
fn start<'a>(
    r: &Reader<'a>,
    e: &BytesStart<'a>,
    empty: bool,
    rw: &Rewrite,
    place: Place,
    walk: &mut Walk,
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
    if walk.bare.before(rw, (ns, local), empty, buf) {
        return Ok(());
    }
    let mut attrs = r.attrs(e)?;
    let mut changed = rw.attrs(ns, local, &mut attrs);
    if let Some(slot) = ids::slot(ns, local) {
        changed |= walk
            .local
            .renumber(&mut rw.ids.borrow_mut(), &slot, &mut attrs);
    }
    rw.tally.borrow_mut().see(ns, local, &attrs);
    if ns == W && local == "fldChar" {
        walk.field.settle(&rw.names, buf);
    }
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
        walk.bare.make(rw, Block::P, buf);
        buf.extend(format!("</{}>", e.name().as_ref()).as_bytes());
    } else if !empty && let Some(b) = block {
        walk.bare.open = Some((b, false));
    }
    if !empty && ns == W && matches!(local, "instrText" | "delInstrText") {
        walk.field.open(buf.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml::{MC, XMLNS};

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
