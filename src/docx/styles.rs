//! Styles: each style an appended document uses either maps to the output's style of the same
//! type and name or is copied, renamed when the two definitions clash. Identity is the name,
//! never the style id, which Word localizes.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use crate::StyleMode;
use crate::docx::W;
use crate::error::{Error, thousands};
use crate::xml::{self, A, Attr, Child, Node};

/// Word refuses a document with more styles.
const MAX_STYLES: usize = 4_079;

/// Left out of a comparison: they change how a style is listed, not how text looks.
const NOT_COMPARED: [&str; 13] = [
    "name",
    "aliases",
    "uiPriority",
    "semiHidden",
    "unhideWhenUsed",
    "qFormat",
    "locked",
    "personal",
    "personalCompose",
    "personalReply",
    "rsid",
    "hidden",
    "autoRedefine",
];

/// Word ignores what these hold, so each maps to the output's own.
const WORD_IGNORES: [(&str, &str); 3] = [
    ("character", "default paragraph font"),
    ("numbering", "no list"),
    ("table", "normal table"),
];

fn val(n: &Node) -> Option<&str> {
    n.attr(W, "val")
}

fn id_of(s: &Node) -> &str {
    s.attr(W, "styleId").unwrap_or_default()
}

fn type_of(s: &Node) -> &str {
    s.attr(W, "type").unwrap_or("paragraph")
}

fn name_of(s: &Node) -> &str {
    s.child(W, "name").and_then(val).unwrap_or(id_of(s))
}

/// The `w:val` of an element's `local` child.
pub fn reference<'a>(s: &'a Node, local: &str) -> Option<&'a str> {
    s.child(W, local).and_then(val)
}

fn key(s: &Node) -> (String, String) {
    (type_of(s).to_string(), name_of(s).to_lowercase())
}

/// The styles of one input.
#[derive(Default)]
pub struct Sheet {
    pub styles: Vec<Node>,
    by_id: HashMap<String, usize>,
    pub defaults: Option<Node>,
}

impl Sheet {
    pub fn new(root: Option<&Node>) -> Self {
        let mut sheet = Sheet::default();
        for n in root.iter().flat_map(|r| r.nodes()) {
            if n.is(W, "style") {
                sheet.by_id.insert(id_of(n).into(), sheet.styles.len());
                sheet.styles.push(n.clone());
            } else if n.is(W, "docDefaults") {
                sheet.defaults = Some(n.clone());
            }
        }
        sheet
    }

    fn get(&self, id: &str) -> Option<&Node> {
        self.by_id.get(id).map(|&i| &self.styles[i])
    }

    /// The id of the default style of a type.
    pub fn default_of(&self, kind: &str) -> Option<&str> {
        let on = |s: &&Node| matches!(s.attr(W, "default"), Some("1" | "true" | "on"));
        self.styles
            .iter()
            .filter(|s| type_of(s) == kind)
            .find(on)
            .map(id_of)
    }

    pub fn ids(&self) -> HashSet<String> {
        self.by_id.keys().cloned().collect()
    }

    /// Whether a style gives its paragraphs an outline level, itself or through `basedOn`.
    pub fn outlined(&self, id: &str) -> bool {
        let mut seen = HashSet::new();
        let mut at = Some(id);
        while let Some(id) = at.filter(|id| seen.insert(*id)) {
            let Some(s) = self.get(id) else { break };
            let level = s.child(W, "pPr").and_then(|p| p.child(W, "outlineLvl"));
            if let Some(level) = level {
                return val(level) != Some("9");
            }
            at = reference(s, "basedOn");
        }
        false
    }

    /// `seeds` and every style they reach through `basedOn`, `link` and `next`, and through
    /// what `numbering` says a style's list reaches.
    pub fn closure(
        &self,
        seeds: &BTreeSet<String>,
        numbering: &dyn Fn(&str) -> Vec<String>,
    ) -> BTreeSet<String> {
        let mut seen = seeds.clone();
        let mut pending: Vec<String> = seeds.iter().cloned().collect();
        while let Some(id) = pending.pop() {
            let Some(s) = self.get(&id) else { continue };
            let mut next: Vec<String> = ["basedOn", "link", "next"]
                .iter()
                .filter_map(|l| reference(s, l).map(String::from))
                .collect();
            s.walk(&mut |n| {
                if n.is(W, "numId")
                    && let Some(v) = val(n)
                {
                    next.extend(numbering(v));
                }
            });
            for n in next {
                if seen.insert(n.clone()) {
                    pending.push(n);
                }
            }
        }
        seen
    }
}

/// What the numbering of a comparison looks like, by `w:numId`.
pub type ListForm<'a> = &'a dyn Fn(&str) -> String;

/// What a style's definition looks like with its references by name, for comparing. `based`
/// names a `basedOn` target, `named` the targets of `next` and `link`.
fn canon(
    s: &Node,
    based: &dyn Fn(&str) -> String,
    named: &dyn Fn(&str) -> String,
    lists: ListForm,
    theme: Option<&Theme>,
) -> String {
    let mut n = s.clone();
    n.children.retain(
        |c| !matches!(c, Child::Node(x) if x.ns == W && NOT_COMPARED.contains(&x.local.as_str())),
    );
    n.attrs.retain(|a| !(a.ns == W && a.local == "styleId"));
    n.walk_mut(&mut |x| {
        if let Some(t) = theme {
            t.resolve(&x.ns, &x.local, &mut x.attrs);
        }
        if x.ns != W {
            return;
        }
        let form: &dyn Fn(&str) -> String = match x.local.as_str() {
            "basedOn" => based,
            "next" | "link" => named,
            "numId" => lists,
            _ => return,
        };
        if let Some(a) = x.attrs.iter_mut().find(|a| a.ns == W && a.local == "val") {
            a.value = form(&a.value);
        }
    });
    let mut out = String::new();
    n.canonical(&mut out);
    out
}

/// How one document's styles landed in the output.
#[derive(Default)]
pub struct Mapped {
    /// Old style id to output style id.
    pub ids: HashMap<String, String>,
    /// Which of [`Output::styles`] are this document's copies.
    pub copied: Range<usize>,
    /// (old name, new name).
    pub renames: Vec<(String, String)>,
}

/// One document's side of a mapping.
pub struct Source<'a> {
    pub sheet: &'a Sheet,
    pub mode: StyleMode,
    /// The input's 1-based position.
    pub position: usize,
    pub lists: ListForm<'a>,
    /// The output's numbering, by the same form.
    pub output_lists: ListForm<'a>,
    /// This document's theme and the output's, when they differ and links come off.
    pub themes: Option<(&'a Theme, &'a Theme)>,
}

/// The output's styles: the base's and, after them, the copies.
#[derive(Default)]
pub struct Output {
    pub styles: Vec<Node>,
    names: HashMap<(String, String), usize>,
    ids: HashSet<String>,
    by_id: HashMap<String, usize>,
    base: usize,
}

impl Output {
    pub fn new(sheet: Sheet) -> Self {
        let mut out = Output::default();
        for s in sheet.styles {
            out.register(s);
        }
        out.base = out.styles.len();
        out
    }

    fn register(&mut self, s: Node) {
        let at = self.styles.len();
        self.names.entry(key(&s)).or_insert(at);
        self.ids.insert(id_of(&s).to_lowercase());
        self.by_id.entry(id_of(&s).into()).or_insert(at);
        self.styles.push(s);
    }

    pub fn ids(&self) -> HashSet<String> {
        self.by_id.keys().cloned().collect()
    }

    /// The id of the output's style of a type and (lower-case) name.
    pub fn id_named(&self, kind: &str, name: &str) -> Option<&str> {
        let key = (kind.to_string(), name.to_string());
        self.names.get(&key).map(|&i| id_of(&self.styles[i]))
    }

    /// The styles copied so far.
    pub fn added(&self) -> &[Node] {
        &self.styles[self.base..]
    }

    /// A style's name in lower case, by id (or the id when no style has it).
    fn shown(&self, id: &str) -> String {
        self.by_id.get(id).map_or(id.to_lowercase(), |&i| {
            name_of(&self.styles[i]).to_lowercase()
        })
    }

    /// Maps every style in `used`, parents first.
    pub fn map(&mut self, src: &Source, used: &BTreeSet<String>) -> Result<Mapped, Error> {
        let start = self.styles.len();
        let mut mapped = Mapped::default();
        for id in used {
            let mut chain = vec![];
            let mut seen = HashSet::new();
            let mut at = Some(id.as_str());
            while let Some(s) = at.and_then(|i| src.sheet.get(i)) {
                if mapped.ids.contains_key(id_of(s)) || !seen.insert(id_of(s)) {
                    break;
                }
                chain.push(s);
                at = reference(s, "basedOn");
            }
            for s in chain.into_iter().rev() {
                self.place(s, src, &mut mapped);
            }
        }
        if self.styles.len() > MAX_STYLES {
            let n = thousands(self.styles.len() as u64);
            let what = "The merged document's style count";
            return Err(Error::limit_exceeded(
                what,
                &n,
                &thousands(MAX_STYLES as u64),
            ));
        }
        mapped.copied = start..self.styles.len();
        Ok(mapped)
    }

    fn place(&mut self, s: &Node, src: &Source, mapped: &mut Mapped) {
        let k = key(s);
        let ignored = WORD_IGNORES.contains(&(k.0.as_str(), k.1.as_str()));
        let id = id_of(s).to_string();
        let new = match self.names.get(&k).copied() {
            Some(i) if ignored || src.mode == StyleMode::First || self.equal(i, s, src, mapped) => {
                id_of(&self.styles[i]).to_string()
            }
            Some(_) => self.copy(s, src.position, true, mapped),
            None => self.copy(s, src.position, false, mapped),
        };
        mapped.ids.insert(id, new);
    }

    fn equal(&self, i: usize, s: &Node, src: &Source, mapped: &Mapped) -> bool {
        let shown = |id: &str| self.shown(id);
        let (theirs, base) = src.themes.unzip();
        let ours = canon(&self.styles[i], &shown, &shown, src.output_lists, base);
        let own = |id: &str| {
            src.sheet
                .get(id)
                .map_or(id.to_lowercase(), |s| name_of(s).to_lowercase())
        };
        let based = |id: &str| mapped.ids.get(id).map_or(own(id), |new| self.shown(new));
        ours == canon(s, &based, &own, src.lists, theirs)
    }

    /// Adds a copy of `s` and returns its id. On a clash it gets a new name and id; an id taken
    /// by another style is changed either way. The number is the lowest free one from the
    /// input's position.
    fn copy(&mut self, s: &Node, position: usize, clash: bool, mapped: &mut Mapped) -> String {
        let (name, id, kind) = (name_of(s), id_of(s), type_of(s));
        let free_name = |n: &str| !self.names.contains_key(&(kind.into(), n.to_lowercase()));
        let free_id = |i: &str| !self.ids.contains(&i.to_lowercase());
        let (new_name, new_id) = if clash || !free_id(id) {
            let k = (position..)
                .find(|k| {
                    free_id(&format!("{id}_{k}")) && (!clash || free_name(&format!("{name} ({k})")))
                })
                .expect("a free number");
            let new_name = if clash {
                // Word stores built-in names in lower case and shows them capitalized; a new
                // name is shown as stored.
                let mut shown = name.chars();
                let first = shown.next().map(|c| c.to_uppercase().to_string());
                format!("{}{} ({k})", first.unwrap_or_default(), shown.as_str())
            } else {
                name.to_string()
            };
            (new_name, format!("{id}_{k}"))
        } else {
            (name.to_string(), id.to_string())
        };
        let mut n = s.clone();
        // The output has one default style of each type: the base's.
        n.attrs.retain(|a| !(a.ns == W && a.local == "default"));
        n.set_attr(W, "w", "styleId", &new_id);
        if new_name != name {
            mapped.renames.push((name.to_string(), new_name.clone()));
            match n.child_mut(W, "name") {
                Some(c) => c.set_attr(W, "w", "val", &new_name),
                None => {
                    let attr = Attr::new("w:val", W, &new_name);
                    let child = Node::new("w:name", W, vec![attr]);
                    n.children.insert(0, Child::Node(child));
                }
            }
        }
        self.register(n);
        new_id
    }
}

/// A document's theme, as far as text needs it.
pub struct Theme {
    /// The canonical form of its colours and fonts.
    form: String,
    /// `minorHAnsi` and the like to typefaces.
    fonts: HashMap<String, String>,
}

/// The script a language's theme font is listed under.
fn script(lang: &str) -> Option<&'static str> {
    let (base, region) = lang.split_once('-').unwrap_or((lang, ""));
    Some(match base {
        "ja" => "Jpan",
        "ko" => "Hang",
        "zh" if ["TW", "HK", "MO"].contains(&region) => "Hant",
        "zh" => "Hans",
        "ar" | "fa" | "ur" => "Arab",
        "he" => "Hebr",
        "th" => "Thai",
        _ => return None,
    })
}

impl Theme {
    /// `lang` is the document's `w:themeFontLang`.
    pub fn new(root: &Node, lang: Option<&Node>) -> Self {
        let elements = root.child(A, "themeElements");
        let form = xml::theme_form(root);
        let mut fonts = HashMap::new();
        let scheme = elements.and_then(|e| e.child(A, "fontScheme"));
        for (family, word) in [("majorFont", "major"), ("minorFont", "minor")] {
            let Some(f) = scheme.and_then(|s| s.child(A, family)) else {
                continue;
            };
            let face = |local: &str| {
                let n = f.child(A, local);
                n.and_then(|n| n.attr("", "typeface"))
                    .unwrap_or_default()
                    .to_string()
            };
            let scripted = |attr: &str, local: &str| {
                let wanted = lang.and_then(|l| l.attr(W, attr)).and_then(script);
                let listed = f.nodes().find(|n| {
                    n.is(A, "font") && wanted.is_some() && n.attr("", "script") == wanted
                });
                listed
                    .and_then(|n| n.attr("", "typeface"))
                    .map_or(face(local), String::from)
            };
            fonts.insert(format!("{word}Ascii"), face("latin"));
            fonts.insert(format!("{word}HAnsi"), face("latin"));
            fonts.insert(format!("{word}EastAsia"), scripted("eastAsia", "ea"));
            fonts.insert(format!("{word}Bidi"), scripted("bidi", "cs"));
        }
        Self { form, fonts }
    }

    pub fn differs(&self, other: &Theme) -> bool {
        self.form != other.form
    }

    /// Takes the theme links off an element, leaving the colours Word wrote beside them and
    /// this theme's typefaces. True when something changed.
    pub fn resolve(&self, ns: &str, local: &str, attrs: &mut Vec<Attr>) -> bool {
        const COLOUR: [&str; 6] = [
            "themeColor",
            "themeTint",
            "themeShade",
            "themeFill",
            "themeFillTint",
            "themeFillShade",
        ];
        let before = attrs.len();
        attrs.retain(|a| !(a.ns == W && COLOUR.contains(&a.local.as_str())));
        let mut changed = attrs.len() != before;
        if ns != W || local != "rFonts" {
            return changed;
        }
        for (linked, plain) in [
            ("asciiTheme", "ascii"),
            ("hAnsiTheme", "hAnsi"),
            ("eastAsiaTheme", "eastAsia"),
            ("cstheme", "cs"),
        ] {
            let Some(i) = attrs.iter().position(|a| a.ns == W && a.local == linked) else {
                continue;
            };
            let a = attrs.remove(i);
            changed = true;
            attrs.retain(|x| !(x.ns == W && x.local == plain));
            if let Some(face) = self.fonts.get(&a.value).filter(|f| !f.is_empty()) {
                attrs.push(Attr::new(&a.key.replace(linked, plain), W, face));
            }
        }
        changed
    }
}

/// `w:pPr` children in schema order.
const PPR: [&str; 36] = [
    "pStyle",
    "keepNext",
    "keepLines",
    "pageBreakBefore",
    "framePr",
    "widowControl",
    "numPr",
    "suppressLineNumbers",
    "pBdr",
    "shd",
    "tabs",
    "suppressAutoHyphens",
    "kinsoku",
    "wordWrap",
    "overflowPunct",
    "topLinePunct",
    "autoSpaceDE",
    "autoSpaceDN",
    "bidi",
    "adjustRightInd",
    "snapToGrid",
    "spacing",
    "ind",
    "contextualSpacing",
    "mirrorIndents",
    "suppressOverlap",
    "jc",
    "textDirection",
    "textAlignment",
    "textboxTightWrap",
    "outlineLvl",
    "divId",
    "cnfStyle",
    "rPr",
    "sectPr",
    "pPrChange",
];

/// `w:rPr` children in schema order; a name not listed (another namespace's extension) goes
/// just before the last.
const RPR: [&str; 40] = [
    "rStyle",
    "rFonts",
    "b",
    "bCs",
    "i",
    "iCs",
    "caps",
    "smallCaps",
    "strike",
    "dstrike",
    "outline",
    "shadow",
    "emboss",
    "imprint",
    "noProof",
    "snapToGrid",
    "vanish",
    "webHidden",
    "color",
    "spacing",
    "w",
    "kern",
    "position",
    "sz",
    "szCs",
    "highlight",
    "u",
    "effect",
    "bdr",
    "shd",
    "fitText",
    "vertAlign",
    "rtl",
    "cs",
    "em",
    "lang",
    "eastAsianLayout",
    "specVanish",
    "oMath",
    "rPrChange",
];

/// `w:style` children in schema order, up to where a new `w:pPr` or `w:rPr` can go.
const STYLE: [&str; 22] = [
    "name",
    "aliases",
    "basedOn",
    "next",
    "link",
    "autoRedefine",
    "hidden",
    "uiPriority",
    "semiHidden",
    "unhideWhenUsed",
    "qFormat",
    "locked",
    "personal",
    "personalCompose",
    "personalReply",
    "rsid",
    "pPr",
    "rPr",
    "tblPr",
    "trPr",
    "tcPr",
    "tblStylePr",
];

/// Attributes that say the same thing, so one already set keeps the others out.
const SAME_MEANING: [&[&str]; 8] = [
    &["ascii", "asciiTheme"],
    &["hAnsi", "hAnsiTheme"],
    &["eastAsia", "eastAsiaTheme"],
    &["cs", "cstheme"],
    &["firstLine", "hanging", "firstLineChars", "hangingChars"],
    &["line", "lineRule"],
    &["before", "beforeLines", "beforeAutospacing"],
    &["after", "afterLines", "afterAutospacing"],
];

type Fallback<'a> = (&'a str, &'a [(&'a str, &'a str)]);

/// What Word uses when nothing sets these properties.
const WORD_PPR: [Fallback; 3] = [
    (
        "spacing",
        &[
            ("before", "0"),
            ("after", "0"),
            ("line", "240"),
            ("lineRule", "auto"),
        ],
    ),
    ("ind", &[("left", "0"), ("right", "0"), ("firstLine", "0")]),
    ("jc", &[("val", "left")]),
];
const WORD_RPR: [Fallback; 3] = [
    (
        "rFonts",
        &[
            ("ascii", "Times New Roman"),
            ("hAnsi", "Times New Roman"),
            ("cs", "Times New Roman"),
        ],
    ),
    ("sz", &[("val", "20")]),
    ("szCs", &[("val", "20")]),
];

/// Puts `child` into `parent` where `order` says it belongs.
fn insert_ordered(parent: &mut Node, child: Node, order: &[&str]) {
    let rank = |n: &Node| {
        let listed = order.iter().position(|o| n.ns == W && n.local == *o);
        listed.unwrap_or(order.len() - 2)
    };
    let at = parent
        .children
        .iter()
        .position(|c| matches!(c, Child::Node(n) if rank(n) > rank(&child)))
        .unwrap_or(parent.children.len());
    parent.children.insert(at, Child::Node(child));
}

fn default_props(defaults: Option<&Node>, outer: &str, inner: &str) -> Vec<Node> {
    let props = defaults
        .and_then(|d| d.child(W, outer))
        .and_then(|o| o.child(W, inner));
    props.map_or(vec![], |p| p.nodes().cloned().collect())
}

/// Adds `props` under a style's own `local` element: what the style sets stays, and of a
/// shared element it takes only the attributes it lacks.
fn fold_into(s: &mut Node, local: &str, props: &[Node], order: &[&str]) {
    if props.is_empty() {
        return;
    }
    if s.child(W, local).is_none() {
        let prefix = s.name.strip_suffix("style").unwrap_or("w:");
        let n = Node::new(&format!("{prefix}{local}"), W, vec![]);
        insert_ordered(s, n, &STYLE);
    }
    let own = s.child_mut(W, local).expect("added above");
    for p in props {
        match own.child_mut(W, &p.local) {
            Some(mine) if ["spacing", "ind", "rFonts"].contains(&p.local.as_str()) => {
                for a in p.attrs.iter().filter(|a| !a.is_declaration()) {
                    let group = SAME_MEANING.iter().find(|g| g.contains(&a.local.as_str()));
                    let set = mine.attrs.iter().any(|b| {
                        b.local == a.local || group.is_some_and(|g| g.contains(&b.local.as_str()))
                    });
                    if !set {
                        mine.attrs.push(a.clone());
                    }
                }
            }
            Some(_) => {}
            None => insert_ordered(own, p.clone(), order),
        }
    }
}

/// Adds to `own` what Word uses for each property of `table` that only `theirs` sets.
fn fall_back(own: &mut Vec<Node>, theirs: &[Node], table: &[Fallback]) {
    for (local, attrs) in table {
        let has = |list: &[Node]| list.iter().any(|n| n.local == *local);
        if has(theirs) && !has(own) {
            let attrs = attrs
                .iter()
                .map(|(k, v)| Attr::new(&format!("w:{k}"), W, v))
                .collect();
            own.push(Node::new(&format!("w:{local}"), W, attrs));
        }
    }
}

impl Sheet {
    /// Whether this document's defaults say something else than `base`.
    pub fn defaults_differ(&self, base: Option<&Node>) -> bool {
        let form = |d: Option<&Node>| {
            let mut s = String::new();
            if let Some(d) = d {
                d.canonical(&mut s);
            }
            s
        };
        form(self.defaults.as_ref()) != form(base)
    }

    /// Moves this document's defaults into its paragraph styles with no parent, so they hold
    /// under the base's defaults. What only the base's defaults set comes in as what Word uses
    /// when nothing sets it.
    pub fn fold_defaults(&mut self, base: Option<&Node>) {
        let mut ppr = default_props(self.defaults.as_ref(), "pPrDefault", "pPr");
        let mut rpr = default_props(self.defaults.as_ref(), "rPrDefault", "rPr");
        fall_back(
            &mut ppr,
            &default_props(base, "pPrDefault", "pPr"),
            &WORD_PPR,
        );
        fall_back(
            &mut rpr,
            &default_props(base, "rPrDefault", "rPr"),
            &WORD_RPR,
        );
        for s in &mut self.styles {
            if type_of(s) == "paragraph" && reference(s, "basedOn").is_none() {
                fold_into(s, "pPr", &ppr, &PPR);
                fold_into(s, "rPr", &rpr, &RPR);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Origin, xml};

    const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn sheet(styles: &str) -> Sheet {
        let text = format!("<w:styles {NS}>{styles}</w:styles>");
        let root = xml::tree(&text, "word/styles.xml", &Origin::test("x.docx")).unwrap();
        Sheet::new(Some(&root))
    }

    fn style(kind: &str, id: &str, name: &str, rest: &str) -> String {
        format!(
            r#"<w:style w:type="{kind}" w:styleId="{id}"><w:name w:val="{name}"/>{rest}</w:style>"#
        )
    }

    fn mapped(out: &mut Output, doc: &Sheet, mode: StyleMode, ids: &[&str]) -> Mapped {
        let plain = |n: &str| n.to_string();
        let src = Source {
            sheet: doc,
            mode,
            position: 2,
            lists: &plain,
            output_lists: &plain,
            themes: None,
        };
        let seeds: BTreeSet<String> = ids.iter().map(|s| s.to_string()).collect();
        out.map(&src, &doc.closure(&seeds, &|_| vec![])).unwrap()
    }

    #[test]
    fn localized_ids_match_by_name() {
        let h1 = r#"<w:rPr><w:b/></w:rPr>"#;
        let base = sheet(&style("paragraph", "Heading1", "heading 1", h1));
        let doc = sheet(&style("paragraph", "berschrift1", "Heading 1", h1));
        let mut out = Output::new(base);
        let m = mapped(&mut out, &doc, StyleMode::Keep, &["berschrift1"]);
        assert_eq!(m.ids["berschrift1"], "Heading1");
        assert!(out.added().is_empty() && m.renames.is_empty());
    }

    #[test]
    fn what_only_lists_a_style_doesnt_make_it_differ() {
        let base = sheet(&style(
            "paragraph",
            "A",
            "A",
            r#"<w:uiPriority w:val="1"/><w:rsid w:val="00AA"/><w:rPr><w:b/></w:rPr>"#,
        ));
        let doc = sheet(&style(
            "paragraph",
            "A",
            "A",
            r#"<w:qFormat/><w:rPr>
            <w:b/></w:rPr>"#,
        ));
        let mut out = Output::new(base);
        assert!(
            mapped(&mut out, &doc, StyleMode::Keep, &["A"])
                .renames
                .is_empty()
        );
    }

    #[test]
    fn a_clash_spreads_down_a_chain() {
        let red = r#"<w:rPr><w:color w:val="FF0000"/></w:rPr>"#;
        let blue = r#"<w:rPr><w:color w:val="0000FF"/></w:rPr>"#;
        let child = r#"<w:basedOn w:val="Normal"/><w:rPr><w:b/></w:rPr>"#;
        let base = sheet(
            &(style("paragraph", "Normal", "Normal", red)
                + &style("paragraph", "H", "Head", child)),
        );
        let doc = sheet(
            &(style("paragraph", "Normal", "Normal", blue)
                + &style("paragraph", "H", "Head", child)),
        );
        let mut out = Output::new(base);
        let m = mapped(&mut out, &doc, StyleMode::Keep, &["H"]);
        assert_eq!(m.ids["H"], "H_2");
        assert_eq!(m.ids["Normal"], "Normal_2");
        assert_eq!(
            m.renames,
            [
                ("Normal".to_string(), "Normal (2)".to_string()),
                ("Head".to_string(), "Head (2)".to_string())
            ]
        );
    }

    #[test]
    fn first_mode_maps_by_name_and_copies_only_what_is_missing() {
        let base = sheet(&style("paragraph", "A", "A", "<w:rPr><w:b/></w:rPr>"));
        let doc = sheet(
            &(style("paragraph", "A", "A", "<w:rPr><w:i/></w:rPr>")
                + &style("paragraph", "B", "B", "")),
        );
        let mut out = Output::new(base);
        let m = mapped(&mut out, &doc, StyleMode::First, &["A", "B"]);
        assert_eq!(m.ids["A"], "A");
        assert_eq!(m.ids["B"], "B");
        assert_eq!(out.added().len(), 1);
        assert!(m.renames.is_empty());
    }

    #[test]
    fn an_id_used_by_another_type_is_changed_first() {
        let base = sheet(&style("character", "Foo", "Foo Char", ""));
        let doc = sheet(&style("paragraph", "FOO", "Foo", ""));
        let mut out = Output::new(base);
        let m = mapped(&mut out, &doc, StyleMode::First, &["FOO"]);
        assert_eq!(m.ids["FOO"], "FOO_2");
        assert!(m.renames.is_empty(), "the name stays");
        assert_eq!(name_of(&out.added()[0]), "Foo");
    }

    #[test]
    fn styles_word_ignores_map_to_the_outputs() {
        let base = sheet(&style("table", "TableNormal", "Normal Table", "<w:tblPr/>"));
        let doc = sheet(&style(
            "table",
            "NormaleTabelle",
            "Normal Table",
            "<w:rsid/><w:pPr/>",
        ));
        let mut out = Output::new(base);
        let m = mapped(&mut out, &doc, StyleMode::Keep, &["NormaleTabelle"]);
        assert_eq!(m.ids["NormaleTabelle"], "TableNormal");
    }

    #[test]
    fn numbers_skip_names_already_taken() {
        let blue = "<w:rPr><w:i/></w:rPr>";
        let base =
            sheet(&(style("paragraph", "A", "A", "") + &style("paragraph", "A_2", "A (2)", "")));
        let doc = sheet(&style("paragraph", "A", "A", blue));
        let mut out = Output::new(base);
        let m = mapped(&mut out, &doc, StyleMode::Keep, &["A"]);
        assert_eq!(m.ids["A"], "A_3");
        assert_eq!(m.renames[0].1, "A (3)");
    }

    fn folded(defaults: &str, base: &str, own: &str) -> String {
        let root = |d: &str| {
            let text = format!("<w:styles {NS}><w:docDefaults>{d}</w:docDefaults></w:styles>");
            xml::tree(&text, "s.xml", &Origin::test("x.docx")).unwrap()
        };
        let base = root(base);
        let mut doc = sheet(&format!(
            "<w:docDefaults>{defaults}</w:docDefaults>{}",
            style("paragraph", "N", "Normal", own)
        ));
        doc.fold_defaults(base.child(W, "docDefaults"));
        let mut out = String::new();
        let s = &doc.styles[0];
        for n in s.nodes().filter(|n| n.local != "name") {
            n.write(&mut out);
        }
        out
    }

    #[test]
    fn defaults_go_under_a_styles_own_properties_in_schema_order() {
        let defaults = r#"<w:rPrDefault><w:rPr><w:rFonts w:asciiTheme="minorHAnsi" w:cs="Arial"/><w:sz w:val="24"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="278" w:lineRule="auto"/></w:pPr></w:pPrDefault>"#;
        let own = r#"<w:pPr><w:keepNext/><w:spacing w:lineRule="exact" w:before="10"/><w:jc w:val="right"/></w:pPr><w:rPr><w:rFonts w:ascii="Arial"/><w:b/><w:color w:val="FF0000"/><w:lang w:val="en-GB"/></w:rPr>"#;
        assert_eq!(
            folded(defaults, defaults, own),
            r#"<w:pPr><w:keepNext/><w:spacing w:lineRule="exact" w:before="10" w:after="160"/><w:jc w:val="right"/></w:pPr><w:rPr><w:rFonts w:ascii="Arial" w:cs="Arial"/><w:b/><w:color w:val="FF0000"/><w:sz w:val="24"/><w:lang w:val="en-GB"/></w:rPr>"#
        );
    }

    #[test]
    fn what_only_the_base_defaults_set_falls_back_to_words_own_values() {
        let base = r#"<w:rPrDefault><w:rPr><w:sz w:val="22"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:jc w:val="both"/></w:pPr></w:pPrDefault>"#;
        assert_eq!(
            folded("", base, ""),
            r#"<w:pPr><w:jc w:val="left"/></w:pPr><w:rPr><w:sz w:val="20"/></w:rPr>"#
        );
    }

    fn theme(fonts: &str, lang: &str) -> Theme {
        let text = format!(
            r#"<a:theme xmlns:a="{A}"><a:themeElements><a:clrScheme name="x"><a:dk1/></a:clrScheme><a:fontScheme name="y">{fonts}</a:fontScheme></a:themeElements></a:theme>"#
        );
        let o = Origin::test("x.docx");
        let root = xml::tree(&text, "t.xml", &o).unwrap();
        let settings = format!("<w:themeFontLang {NS} {lang}/>");
        let lang = xml::tree(&settings, "s.xml", &o).unwrap();
        Theme::new(&root, Some(&lang))
    }

    #[test]
    fn theme_links_become_the_documents_own_values() {
        let fonts = r#"<a:majorFont><a:latin typeface="Head"/><a:ea typeface=""/><a:cs typeface=""/><a:font script="Jpan" typeface="Mincho"/></a:majorFont><a:minorFont><a:latin typeface="Body"/><a:ea typeface=""/><a:cs typeface="Bidi"/></a:minorFont>"#;
        let t = theme(fonts, r#"w:eastAsia="ja-JP""#);
        let attr = |k: &str, v: &str| Attr::new(&format!("w:{k}"), W, v);
        let mut attrs = vec![
            attr("ascii", "Old"),
            attr("asciiTheme", "majorHAnsi"),
            attr("eastAsiaTheme", "majorEastAsia"),
            attr("cstheme", "minorBidi"),
            attr("hAnsiTheme", "majorBidi"),
        ];
        assert!(t.resolve(W, "rFonts", &mut attrs));
        let got: Vec<_> = attrs
            .iter()
            .map(|a| (a.key.as_str(), a.value.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("w:ascii", "Head"),
                ("w:eastAsia", "Mincho"),
                ("w:cs", "Bidi")
            ],
            "an empty typeface leaves the attribute out"
        );
        let mut attrs = vec![
            attr("val", "0F4761"),
            attr("themeColor", "accent1"),
            attr("themeShade", "BF"),
        ];
        assert!(t.resolve(W, "color", &mut attrs));
        assert_eq!(attrs, [attr("val", "0F4761")]);
        assert!(!t.resolve(W, "color", &mut attrs));
        assert!(t.differs(&theme("<a:minorFont/>", "")));
        assert!(!t.differs(&theme(fonts, "")));
    }

    #[test]
    fn theme_links_compare_by_what_they_show_when_themes_differ() {
        let linked = r#"<w:rPr><w:color w:val="111111" w:themeColor="accent1"/></w:rPr>"#;
        let shown = r#"<w:rPr><w:color w:val="111111"/></w:rPr>"#;
        let (a, b) = (theme("", ""), theme("<a:minorFont/>", ""));
        let plain = |n: &str| n.to_string();
        let doc = sheet(&style("paragraph", "H", "H", shown));
        let src = Source {
            sheet: &doc,
            mode: StyleMode::Keep,
            position: 2,
            lists: &plain,
            output_lists: &plain,
            themes: Some((&a, &b)),
        };
        let mut out = Output::new(sheet(&style("paragraph", "H", "H", linked)));
        let m = out.map(&src, &BTreeSet::from(["H".to_string()])).unwrap();
        assert_eq!(m.ids["H"], "H");
    }

    #[test]
    fn more_than_word_allows_is_refused() {
        let many: String = (0..MAX_STYLES)
            .map(|i| style("paragraph", &format!("S{i}"), &format!("S{i}"), ""))
            .collect();
        let doc = sheet(&style("paragraph", "New", "New", ""));
        let mut out = Output::new(sheet(&many));
        let plain = |n: &str| n.to_string();
        let src = Source {
            sheet: &doc,
            mode: StyleMode::Keep,
            position: 2,
            lists: &plain,
            output_lists: &plain,
            themes: None,
        };
        let e = out
            .map(&src, &BTreeSet::from(["New".to_string()]))
            .err()
            .unwrap();
        assert_eq!(e.code, "limit_exceeded");
        assert!(e.message.contains("4,080"), "{}", e.message);
    }
}
