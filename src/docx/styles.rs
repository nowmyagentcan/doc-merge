//! Styles: each style an appended document uses either maps to the output's style of the same
//! type and name or is copied, renamed when the two definitions clash. Identity is the name,
//! never the style id, which Word localizes.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use crate::StyleMode;
use crate::docx::W;
use crate::error::{Error, thousands};
use crate::xml::{Attr, Child, Node};

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

fn reference<'a>(s: &'a Node, local: &str) -> Option<&'a str> {
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
) -> String {
    let mut n = s.clone();
    n.children.retain(
        |c| !matches!(c, Child::Node(x) if x.ns == W && NOT_COMPARED.contains(&x.local.as_str())),
    );
    n.attrs.retain(|a| !(a.ns == W && a.local == "styleId"));
    n.walk_mut(&mut |x| {
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
        let ours = canon(&self.styles[i], &shown, &shown, src.output_lists);
        let own = |id: &str| {
            src.sheet
                .get(id)
                .map_or(id.to_lowercase(), |s| name_of(s).to_lowercase())
        };
        let based = |id: &str| mapped.ids.get(id).map_or(own(id), |new| self.shown(new));
        ours == canon(s, &based, &own, src.lists)
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
        };
        let e = out
            .map(&src, &BTreeSet::from(["New".to_string()]))
            .err()
            .unwrap();
        assert_eq!(e.code, "limit_exceeded");
        assert!(e.message.contains("4,080"), "{}", e.message);
    }
}
