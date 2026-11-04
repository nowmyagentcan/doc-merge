//! What each document's settings ask for, the fonts, and what stays behind: the base's settings
//! stay, apart from even and odd headers, and an appended document's note formatting moves into
//! its sections.

use std::collections::HashSet;

use super::{
    DIAGRAM_DRAWING, Held, Merger, Parts, REL, Rewrite, Uses, W, copy_rel, held, is_rid, registry,
};
use crate::Origin;
use crate::error::{Error, Warning};
use crate::package::{Package, Rel};
use crate::writer::{Body, Plan, Rels, rels_part};
use crate::xml::{Child, Node, Prefixes, declaration, prefix_of};

/// What comes before `w:evenAndOddHeaders` in `w:settings`.
const BEFORE_EVEN: &str = "\
     writeProtection view zoom removePersonalInformation removeDateAndTime \
     doNotDisplayPageBoundaries displayBackgroundShape printPostScriptOverText \
     printFractionalCharacterWidth printFormsData embedTrueTypeFonts embedSystemFonts \
     saveSubsetFonts saveFormsData mirrorMargins alignBordersAndEdges \
     bordersDoNotSurroundHeader bordersDoNotSurroundFooter gutterAtTop hideSpellingErrors \
     hideGrammaticalErrors activeWritingStyle proofState formsDesign attachedTemplate \
     linkStyles stylePaneFormatFilter stylePaneSortMethod documentType mailMerge revisionView \
     trackRevisions doNotTrackMoves doNotTrackFormatting documentProtection autoFormatOverride \
     styleLockTheme styleLockQFSet defaultTabStop autoHyphenation consecutiveHyphenLimit \
     hyphenationZone doNotHyphenateCaps showEnvelope summaryLength clickAndTypeStyle \
     defaultTableStyle";

/// The layout mode of a document that doesn't name one (Word 2007's).
const OLDEST_MODE: &str = "12";

/// What the merge needs from one document's settings.
pub struct Settings {
    pub even: bool,
    mode: String,
    protected: bool,
    /// `w:footnotePr` and `w:endnotePr` with their formatting only, when they have any.
    notes: [Option<Node>; 2],
}

fn on(n: &Node, attr: &str) -> bool {
    !matches!(n.attr(W, attr), Some("0" | "false" | "off"))
}

fn note_format(settings: &Node, local: &str) -> Option<Node> {
    let mut n = settings.child(W, local)?.clone();
    let format = ["pos", "numFmt", "numStart", "numRestart"];
    n.children
        .retain(|c| matches!(c, Child::Node(c) if format.contains(&c.local.as_str())));
    (!n.children.is_empty()).then_some(n)
}

impl Settings {
    pub fn new(tree: Option<&Node>) -> Self {
        let child = |local| tree.and_then(|t| t.child(W, local));
        let compat = child("compat").into_iter().flat_map(|c| c.nodes());
        let mode = compat
            .filter(|n| n.is(W, "compatSetting"))
            .find(|n| n.attr(W, "name") == Some("compatibilityMode"))
            .and_then(|n| n.attr(W, "val"));
        Self {
            even: child("evenAndOddHeaders").is_some_and(|n| on(n, "val")),
            mode: mode.unwrap_or(OLDEST_MODE).to_string(),
            protected: child("writeProtection").is_some()
                || child("documentProtection")
                    .is_some_and(|n| n.attr(W, "enforcement").is_some() && on(n, "enforcement")),
            notes: ["footnotePr", "endnotePr"].map(|l| tree.and_then(|t| note_format(t, l))),
        }
    }

    /// This document's note formatting where it differs from `base`, written with `w`.
    fn notes_against(&self, base: &Settings, w: &str) -> [Option<String>; 2] {
        let form = |n: &Option<Node>| {
            let mut s = String::new();
            if let Some(n) = n {
                n.canonical(&mut s);
            }
            s
        };
        [0, 1].map(|k| {
            let mut n = self.notes[k]
                .clone()
                .filter(|_| form(&self.notes[k]) != form(&base.notes[k]))?;
            n.spell(W, &format!("{w}:"));
            let mut out = String::new();
            n.write(&mut out);
            Some(out)
        })
    }
}

/// The output's font table.
pub struct Fonts {
    base: Option<Held>,
    root: Prefixes,
    part: String,
    rels: Rels,
    rels_before: usize,
    names: HashSet<String>,
    added: String,
}

fn font_name(n: &Node) -> Option<String> {
    n.is(W, "font")
        .then(|| n.attr(W, "name").map(str::to_lowercase))
        .flatten()
}

impl Fonts {
    pub fn open(pkg: &mut Package, main: &str, rels: &[Rel]) -> Result<Self, Error> {
        let base = held(pkg, main, rels, "fontTable")?;
        let part = base
            .as_ref()
            .map_or("word/fontTable.xml", |h| &h.part)
            .to_string();
        let own = Rels::new(pkg.rels_of(&part)?);
        let trees = base.iter().flat_map(|h| h.tree.nodes());
        Ok(Self {
            names: trees.filter_map(font_name).collect(),
            root: Prefixes::of(base.as_ref().map_or(&[], |h| &h.tree.attrs)),
            base,
            part,
            rels_before: own.list.len(),
            rels: own,
            added: String::new(),
        })
    }

    /// Adds the fonts of input `i` the output lacks, embedded ones with their font files.
    fn add(
        &mut self,
        plan: &mut Plan,
        inputs: &mut [Package],
        i: usize,
        own: &[Rel],
    ) -> Result<(), Error> {
        let main = inputs[i].main.clone();
        let Some(h) = held(&mut inputs[i], &main, own, "fontTable")? else {
            return Ok(());
        };
        let table_rels = inputs[i].rels_of(&h.part)?;
        let local = self.root.merge(&h.tree.attrs);
        for n in h.tree.nodes() {
            if !font_name(n).is_some_and(|name| self.names.insert(name)) {
                continue;
            }
            let mut n = n.clone();
            for embed in n.nodes_mut() {
                for a in embed.attrs.iter_mut().filter(|a| is_rid(a)) {
                    let Some(r) = table_rels.iter().find(|r| r.id == a.value) else {
                        let why =
                            format!("{} points to a missing relationship {}", h.part, a.value);
                        return Err(inputs[i].origin.bad(&why));
                    };
                    let part = &self.part;
                    a.value = copy_rel(plan, inputs, i, &h.part, r, part, &mut self.rels)?.0;
                }
            }
            n.attrs.extend(local.iter().cloned());
            n.write(&mut self.added);
        }
        Ok(())
    }

    fn finish(
        &self,
        plan: &mut Plan,
        rels: &mut Rels,
        base: &Package,
        main: &str,
        origin: &Origin,
    ) -> Result<(), Error> {
        let parts = Parts {
            main,
            kind: "fontTable",
            root: "fonts",
        };
        let inserts: [(&[&str], &str); 1] = [(&[], &self.added)];
        let held = self.base.as_ref();
        registry(plan, rels, &parts, held, &self.root, &inserts, origin)?;
        if self.rels.list.len() > self.rels_before {
            rels_part(plan, base, &self.part, &self.rels);
        }
        Ok(())
    }
}

/// Whether a relationship of a main part is one the merge deals with apart from the content.
fn handled(kind: &str) -> bool {
    kind == DIAGRAM_DRAWING
        || kind.ends_with("/people")
        || kind.rsplit('/').next().is_some_and(|k| {
            matches!(
                k,
                "styles"
                    | "numbering"
                    | "settings"
                    | "webSettings"
                    | "fontTable"
                    | "theme"
                    | "footnotes"
                    | "endnotes"
                    | "comments"
                    | "commentsExtended"
                    | "commentsIds"
                    | "commentsExtensible"
                    | "header"
                    | "footer"
                    | "customXml"
                    | "glossaryDocument"
            )
        })
}

impl Merger<'_> {
    /// Warns about what input `i` loses to the base's settings, and moves its note formatting
    /// into its sections.
    pub(super) fn apply_settings(&mut self, i: usize, rw: &mut Rewrite) {
        let origin = &self.inputs[i].origin;
        let (base, own) = (&self.settings[0], &self.settings[i]);
        if own.mode != base.mode {
            let warning = Warning::compat_mode_differs(i + 1, &origin.name);
            self.warnings.push(warning);
        }
        if own.protected {
            let warning = Warning::protection_not_kept(i + 1, &origin.name);
            self.warnings.push(warning);
        }
        rw.note_format = own.notes_against(base, &rw.w);
    }

    /// Takes input `i`'s fonts and glossary, and warns about the parts that stay behind.
    pub(super) fn document_parts(
        &mut self,
        i: usize,
        own: &[Rel],
        uses: &Uses,
    ) -> Result<(), Error> {
        self.fonts.add(&mut self.plan, self.inputs, i, own)?;
        let origin = self.inputs[i].origin.clone();
        let part = self.inputs[i].main.clone();
        let glossary = format!("{REL}glossaryDocument");
        if let Some(r) = own.iter().find(|r| r.kind == glossary && !r.external) {
            if self.has_glossary {
                let warning = Warning::glossary_not_merged(i + 1, &origin.name);
                self.warnings.push(warning);
            } else {
                let (plan, inputs) = (&mut self.plan, &mut *self.inputs);
                copy_rel(plan, inputs, i, &part, r, &self.main, &mut self.rels)?;
                self.has_glossary = true;
            }
        }
        let mut dropped: Vec<(&str, usize)> = vec![];
        for r in own {
            let used = r.external || handled(&r.kind) || uses.rids.contains(&r.id);
            let kind = r.kind.rsplit('/').next().unwrap_or(&r.kind);
            match dropped.iter_mut().find(|d| d.0 == kind) {
                _ if used => {}
                Some(d) => d.1 += 1,
                None => dropped.push((kind, 1)),
            }
        }
        dropped.sort();
        if !dropped.is_empty() {
            let warning = Warning::part_dropped(i + 1, &origin.name, &dropped);
            self.warnings.push(warning);
        }
        Ok(())
    }

    /// Writes the font table, and the base's settings with even and odd headers on when they
    /// have to change.
    pub(super) fn finish_settings(&mut self, origin: &Origin) -> Result<(), Error> {
        let main = self.main.clone();
        let (plan, rels) = (&mut self.plan, &mut self.rels);
        self.fonts
            .finish(plan, rels, &self.inputs[0], &main, origin)?;
        if !self.needs_twins(0) {
            return Ok(());
        }
        let own = self.inputs[0].rels_of(&main)?;
        let Some(mut h) = held(&mut self.inputs[0], &main, &own, "settings")? else {
            let w = prefix_of(&self.root.attrs, W).unwrap_or("w");
            let setting = format!("<{w}:evenAndOddHeaders/>");
            let parts = Parts {
                main: &main,
                kind: "settings",
                root: "settings",
            };
            let inserts: [(&[&str], &str); 1] = [(&[], &setting)];
            let (plan, rels) = (&mut self.plan, &mut self.rels);
            return registry(plan, rels, &parts, None, &self.root, &inserts, origin);
        };
        let before: Vec<&str> = BEFORE_EVEN.split_whitespace().collect();
        h.tree
            .child_or_insert(W, "evenAndOddHeaders", &before)
            .attrs
            .clear();
        let mut out = declaration(&h.text);
        h.tree.write(&mut out);
        self.plan.replace(&h.part, Body::Data(out.into_bytes()));
        Ok(())
    }
}
