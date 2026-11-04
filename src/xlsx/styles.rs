//! `styles.xml`: the base's lists, then what appended workbooks add. Equal entries are shared
//! and every index is mapped, so a cell keeps its look through its own format.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::S;
use crate::error::{Error, thousands};
use crate::xml::{self, A, Attr, Child, Node, Prefixes};
use crate::{Origin, Rename};

const FIRST_CUSTOM_FORMAT: u32 = 164;
const MAX_CELL_FORMATS: usize = 65_490;
const MAX_FONTS: usize = 512;
/// The top-level elements of a style sheet, in schema order.
const ORDER: [&str; 11] = [
    "numFmts",
    "fonts",
    "fills",
    "borders",
    "cellStyleXfs",
    "cellXfs",
    "cellStyles",
    "dxfs",
    "tableStyles",
    "colors",
    "extLst",
];

/// The colours of `theme="0"` and up.
const SCHEME: [&str; 12] = [
    "lt1", "dk1", "lt2", "dk2", "accent1", "accent2", "accent3", "accent4", "accent5", "accent6",
    "hlink", "folHlink",
];
/// ECMA-376 18.8.27.
const PALETTE: [&str; 64] = [
    "000000", "FFFFFF", "FF0000", "00FF00", "0000FF", "FFFF00", "FF00FF", "00FFFF", "000000",
    "FFFFFF", "FF0000", "00FF00", "0000FF", "FFFF00", "FF00FF", "00FFFF", "800000", "008000",
    "000080", "808000", "800080", "008080", "C0C0C0", "808080", "9999FF", "993366", "FFFFCC",
    "CCFFFF", "660066", "FF8080", "0066CC", "CCCCFF", "000080", "FF00FF", "FFFF00", "00FFFF",
    "800080", "800000", "008080", "0000FF", "00CCFF", "CCFFFF", "CCFFCC", "FFFF99", "99CCFF",
    "FF99CC", "CC99FF", "FFCC99", "3366FF", "33CCCC", "99CC00", "FFCC00", "FF9900", "FF6600",
    "666699", "969696", "003366", "339966", "003300", "333300", "993300", "993366", "333399",
    "333333",
];

/// A style sheet's indexed colours as `RRGGBB`.
fn palette(sheet: Option<&Node>) -> Vec<String> {
    let colors = sheet.and_then(|s| s.child(S, "colors"));
    let custom = colors.and_then(|c| c.child(S, "indexedColors"));
    let mut list: Vec<String> = PALETTE.iter().map(|c| c.to_string()).collect();
    for (at, n) in list.iter_mut().zip(custom.iter().flat_map(|c| c.nodes())) {
        if let Some(rgb) = n.attr("", "rgb") {
            *at = rgb[rgb.len().saturating_sub(6)..].to_string();
        }
    }
    list
}

/// A theme's colours as `RRGGBB`, by theme index.
fn scheme_colours(theme: &Node) -> Vec<String> {
    let elements = theme.child(A, "themeElements");
    let scheme = elements.and_then(|e| e.child(A, "clrScheme"));
    SCHEME
        .iter()
        .map(|name| {
            let colour = scheme.and_then(|s| s.child(A, name));
            let value = colour.and_then(|c| c.nodes().next());
            let rgb = value.and_then(|v| v.attr("", "val").filter(|_| v.local == "srgbClr"));
            let system = value.and_then(|v| v.attr("", "lastClr"));
            rgb.or(system).unwrap_or_default().to_string()
        })
        .collect()
}

/// What a workbook's own theme and palette mean, for when the output's are different.
#[derive(Debug, Default)]
pub struct Colours {
    /// `RRGGBB` by theme index, empty when the themes are the same; an empty entry stays linked.
    theme: Vec<String>,
    /// The major and minor Latin typefaces, when the themes differ.
    fonts: Option<[String; 2]>,
    /// `RRGGBB` by index, empty when the palettes are the same.
    palette: Vec<String>,
}

impl Colours {
    /// What to resolve in a workbook with `theme` and `sheet`, going into an output whose base
    /// has `base_theme` and `base_sheet`; `None` when nothing has to change.
    pub fn new(
        theme: Option<&Node>,
        base_theme: Option<&Node>,
        sheet: Option<&Node>,
        base_sheet: Option<&Node>,
    ) -> Option<Self> {
        let mut colours = Self::default();
        if let Some(t) =
            theme.filter(|t| base_theme.map(xml::theme_form) != Some(xml::theme_form(t)))
        {
            let elements = t.child(A, "themeElements");
            // A colour both themes share keeps its link: Excel lets a table style override
            // a linked font colour, but not a literal one.
            let there = base_theme.map_or(vec![], scheme_colours);
            colours.theme = scheme_colours(t);
            for (own, base) in colours.theme.iter_mut().zip(&there) {
                if own == base {
                    own.clear();
                }
            }
            let face = |family: &str| {
                let fonts = elements.and_then(|e| e.child(A, "fontScheme"));
                let font = fonts.and_then(|f| f.child(A, family));
                let latin = font.and_then(|f| f.child(A, "latin"));
                latin
                    .and_then(|l| l.attr("", "typeface"))
                    .unwrap_or_default()
                    .to_string()
            };
            colours.fonts = Some([face("majorFont"), face("minorFont")]);
        }
        let own = palette(sheet);
        if own != palette(base_sheet) {
            colours.palette = own;
        }
        (colours.themed() || !colours.palette.is_empty()).then_some(colours)
    }

    /// Whether the themes differ.
    pub fn themed(&self) -> bool {
        self.fonts.is_some()
    }

    /// Replaces a colour element's theme or palette reference with the colour it stands for;
    /// a tint stays and applies to that. True when something changed.
    pub fn resolve(&self, attrs: &mut [Attr]) -> bool {
        let mut changed = false;
        for a in attrs.iter_mut().filter(|a| a.ns.is_empty()) {
            let list = match a.local.as_str() {
                "theme" => &self.theme,
                "indexed" => &self.palette,
                _ => continue,
            };
            let at = a.value.parse::<usize>().ok();
            if let Some(rgb) = at.and_then(|i| list.get(i)).filter(|c| !c.is_empty()) {
                *a = Attr::new("rgb", "", &format!("FF{rgb}"));
                changed = true;
            }
        }
        changed
    }

    /// Resolves every colour inside `node`, and a font's link to the theme's fonts.
    fn resolve_all(&self, node: &mut Node) {
        node.walk_mut(&mut |n| {
            self.resolve(&mut n.attrs);
            if let Some(fonts) = self.fonts.as_ref().filter(|_| n.is(S, "font")) {
                self.unlink_font(n, fonts);
            }
        });
    }

    /// Turns `<scheme val="minor"/>` into the typeface it stands for.
    fn unlink_font(&self, font: &mut Node, fonts: &[String; 2]) {
        let Some(scheme) = font.child(S, "scheme") else {
            return;
        };
        let face = match scheme.attr("", "val") {
            Some("major") => fonts[0].clone(),
            Some("minor") => fonts[1].clone(),
            _ => return,
        };
        let at = font
            .children
            .iter()
            .position(|c| matches!(c, Child::Node(n) if n.is(S, "scheme")))
            .expect("the child just found");
        if font.child(S, "name").is_some() {
            font.children.remove(at);
        } else if let Child::Node(n) = &mut font.children[at] {
            n.name = n.name.replace("scheme", "name");
            n.local = "name".into();
        }
        if let Some(name) = font.child_mut(S, "name") {
            name.set_attr("", "", "val", &face);
        }
    }
}

fn entries<'a>(sheet: Option<&'a Node>, list: &str) -> Vec<&'a Node> {
    let list = sheet.and_then(|t| t.child(S, list));
    list.iter().flat_map(|n| n.nodes()).collect()
}

pub fn form(node: &Node) -> String {
    let mut s = String::new();
    node.canonical(&mut s);
    s
}

/// One list of a style sheet: equal entries share an index.
struct Pool {
    list: &'static str,
    index: HashMap<String, u32>,
    len: u32,
    added: Vec<Node>,
}

impl Pool {
    fn new(sheet: Option<&Node>, list: &'static str) -> Self {
        let items = entries(sheet, list);
        Self {
            list,
            len: items.len() as u32,
            index: items
                .iter()
                .enumerate()
                .rev()
                .map(|(i, n)| (form(n), i as u32))
                .collect(),
            added: vec![],
        }
    }

    fn add(&mut self, node: Node) -> u32 {
        *self.index.entry(form(&node)).or_insert_with(|| {
            self.added.push(node);
            self.len += 1;
            self.len - 1
        })
    }
}

/// What an appended workbook's style indices are in the output.
#[derive(Debug, Default)]
pub struct Map {
    /// By cell format.
    pub xfs: Vec<u32>,
    pub dxfs: Vec<u32>,
    /// Custom table styles that got a new name.
    pub table_styles: HashMap<String, String>,
}

impl Map {
    /// The output's cell format for format `i`; a workbook with none has only the default.
    pub fn xf(&self, i: u32) -> Option<u32> {
        match self.xfs.get(i as usize) {
            None if i == 0 => Some(0),
            found => found.copied(),
        }
    }
}

/// The merged style sheet as it grows.
pub struct Output {
    base: Node,
    prefix: String,
    root: Prefixes,
    formats: HashMap<String, u32>,
    next_format: u32,
    new_formats: Vec<Node>,
    fonts: Pool,
    fills: Pool,
    borders: Pool,
    style_xfs: Pool,
    xfs: Pool,
    dxfs: Pool,
    /// Named styles by `builtinId` and by lower-case name, to the cell style format.
    builtin: HashMap<String, u32>,
    named: HashMap<String, u32>,
    new_named: Vec<Node>,
    /// Custom table styles by name, to their canonical form.
    table_styles: HashMap<String, String>,
    new_table_styles: Vec<Node>,
    /// The most cell formats and fonts any one input has.
    largest: [usize; 2],
}

/// A file's custom number formats: id and code.
fn number_formats(sheet: Option<&Node>) -> Vec<(u32, String)> {
    entries(sheet, "numFmts")
        .iter()
        .filter_map(|n| {
            let id = n.attr("", "numFmtId")?.parse().ok()?;
            Some((id, n.attr("", "formatCode")?.to_string()))
        })
        .collect()
}

/// A number attribute of `node` turned into its entry of `map`.
fn remap(node: &mut Node, attr: &str, map: &[u32], origin: &Origin) -> Result<(), Error> {
    let Some(value) = node.attr("", attr) else {
        return Ok(());
    };
    let new = value.parse::<usize>().ok().and_then(|i| map.get(i));
    let Some(new) = new else {
        let why = format!("a cell format has {attr} {value}, which isn't defined");
        return Err(origin.bad(&why));
    };
    let new = new.to_string();
    node.set_attr("", "", attr, &new);
    Ok(())
}

impl Output {
    /// `sheet` is the base's, if it has one.
    pub fn new(sheet: Option<Node>) -> Self {
        let sheet =
            sheet.unwrap_or_else(|| Node::new("styleSheet", S, vec![super::main_namespace()]));
        let base = Some(&sheet);
        let formats = number_formats(base);
        let mut builtin = HashMap::new();
        let mut named = HashMap::new();
        for n in entries(base, "cellStyles") {
            let xf = n.attr("", "xfId").and_then(|v| v.parse().ok()).unwrap_or(0);
            if let Some(id) = n.attr("", "builtinId") {
                builtin.insert(id.to_string(), xf);
            }
            if let Some(name) = n.attr("", "name") {
                named.insert(name.to_lowercase(), xf);
            }
        }
        let table_styles = entries(base, "tableStyles")
            .iter()
            .map(|n| {
                (
                    n.attr("", "name").unwrap_or_default().to_string(),
                    table_style_form(n),
                )
            })
            .collect();
        Self {
            prefix: super::prefix(&sheet).to_string(),
            root: Prefixes::of(&sheet.attrs),
            next_format: formats
                .iter()
                .map(|f| f.0 + 1)
                .max()
                .unwrap_or(0)
                .max(FIRST_CUSTOM_FORMAT),
            formats: formats
                .into_iter()
                .rev()
                .map(|(id, code)| (code, id))
                .collect(),
            new_formats: vec![],
            fonts: Pool::new(base, "fonts"),
            fills: Pool::new(base, "fills"),
            borders: Pool::new(base, "borders"),
            style_xfs: Pool::new(base, "cellStyleXfs"),
            xfs: Pool::new(base, "cellXfs"),
            dxfs: Pool::new(base, "dxfs"),
            largest: [entries(base, "cellXfs").len(), entries(base, "fonts").len()],
            builtin,
            named,
            new_named: vec![],
            table_styles,
            new_table_styles: vec![],
            base: sheet,
        }
    }

    /// The output's id for a custom number format's code.
    fn format_id(&mut self, code: &str) -> u32 {
        if let Some(id) = self.formats.get(code) {
            return *id;
        }
        let id = self.next_format;
        self.next_format += 1;
        self.formats.insert(code.to_string(), id);
        let name = format!("{}numFmt", self.prefix);
        let attrs = vec![
            Attr::new("numFmtId", "", &id.to_string()),
            Attr::new("formatCode", "", code),
        ];
        self.new_formats.push(Node::new(&name, S, attrs));
        id
    }

    /// Takes in an appended workbook's style sheet; returns how its indices map.
    pub fn add(
        &mut self,
        sheet: Option<&Node>,
        colours: Option<&Colours>,
        origin: &Origin,
        renames: &mut Vec<Rename>,
    ) -> Result<Map, Error> {
        let local = self.root.merge(sheet.map_or(&[], |s| &s.attrs));
        let prefix = self.prefix.clone();
        // An entry as the output writes it.
        let adopt = |node: &Node| {
            let mut n = node.clone();
            n.spell(S, &prefix);
            n.attrs.extend(local.iter().cloned());
            if let Some(c) = colours {
                c.resolve_all(&mut n);
            }
            n
        };
        let mut formats = HashMap::new();
        for (id, code) in number_formats(sheet) {
            if id >= FIRST_CUSTOM_FORMAT {
                formats.insert(id, self.format_id(&code));
            }
        }
        let number_format = |n: &mut Node| {
            let id = n.attr("", "numFmtId").and_then(|v| v.parse::<u32>().ok());
            if let Some(id) = id.filter(|id| *id >= FIRST_CUSTOM_FORMAT) {
                let new = formats.get(&id).copied().unwrap_or(0);
                n.set_attr("", "", "numFmtId", &new.to_string());
            }
        };
        let pooled = |pool: &mut Pool| -> Vec<u32> {
            entries(sheet, pool.list)
                .iter()
                .map(|n| pool.add(adopt(n)))
                .collect()
        };
        let fonts = pooled(&mut self.fonts);
        let fills = pooled(&mut self.fills);
        let borders = pooled(&mut self.borders);
        let components = |n: &mut Node| -> Result<(), Error> {
            number_format(n);
            remap(n, "fontId", &fonts, origin)?;
            remap(n, "fillId", &fills, origin)?;
            remap(n, "borderId", &borders, origin)
        };

        // A named style the output has too stays the output's; the others come along.
        let mut known = HashMap::new();
        let mut unknown = vec![];
        for n in entries(sheet, "cellStyles") {
            let xf = n.attr("", "xfId").and_then(|v| v.parse::<u32>().ok());
            let id = n.attr("", "builtinId");
            let name = n.attr("", "name").unwrap_or_default().to_lowercase();
            let there = id
                .and_then(|id| self.builtin.get(id))
                .or(self.named.get(&name));
            match (xf, there) {
                (Some(xf), Some(there)) => {
                    known.insert(xf, *there);
                }
                _ => unknown.push(n),
            }
        }
        let mut style_xfs = vec![];
        for (i, n) in entries(sheet, "cellStyleXfs").iter().enumerate() {
            style_xfs.push(match known.get(&(i as u32)) {
                Some(xf) => *xf,
                None => {
                    let mut n = adopt(n);
                    components(&mut n)?;
                    self.style_xfs.add(n)
                }
            });
        }
        for n in unknown {
            let mut n = adopt(n);
            remap(&mut n, "xfId", &style_xfs, origin)?;
            let xf = n.attr("", "xfId").and_then(|v| v.parse().ok()).unwrap_or(0);
            if let Some(id) = n.attr("", "builtinId") {
                self.builtin.insert(id.to_string(), xf);
            }
            let name = n.attr("", "name").unwrap_or_default().to_lowercase();
            self.named.insert(name, xf);
            self.new_named.push(n);
        }
        let mut xfs = vec![];
        for n in entries(sheet, "cellXfs") {
            let mut n = adopt(n);
            components(&mut n)?;
            remap(&mut n, "xfId", &style_xfs, origin)?;
            xfs.push(self.xfs.add(n));
        }
        self.largest[0] = self.largest[0].max(entries(sheet, "cellXfs").len());
        self.largest[1] = self.largest[1].max(entries(sheet, "fonts").len());
        self.check_limits()?;

        let mut dxfs = vec![];
        for n in entries(sheet, "dxfs") {
            let mut n = adopt(n);
            for inner in n.nodes_mut().filter(|c| c.is(S, "numFmt")) {
                if let Some(code) = inner.attr("", "formatCode").map(String::from) {
                    let id = self.format_id(&code);
                    inner.set_attr("", "", "numFmtId", &id.to_string());
                }
            }
            dxfs.push(self.dxfs.add(n));
        }
        let mut table_styles = HashMap::new();
        for n in entries(sheet, "tableStyles") {
            let mut n = adopt(n);
            let mut failed = None;
            n.walk_mut(&mut |e| {
                if e.is(S, "tableStyleElement")
                    && let Err(error) = remap(e, "dxfId", &dxfs, origin)
                {
                    failed = Some(error);
                }
            });
            if let Some(e) = failed {
                return Err(e);
            }
            let name = n.attr("", "name").unwrap_or_default().to_string();
            let wanted = table_style_form(&n);
            let mut new = name.clone();
            let mut k = 2;
            while self.table_styles.get(&new).is_some_and(|f| *f != wanted) {
                new = format!("{name} {k}");
                k += 1;
            }
            if new != name {
                renames.push(Rename {
                    input: origin.input,
                    kind: "table_style",
                    from: name.clone(),
                    to: new.clone(),
                });
                table_styles.insert(name, new.clone());
            }
            if let Entry::Vacant(free) = self.table_styles.entry(new) {
                n.set_attr("", "", "name", free.key());
                free.insert(wanted);
                self.new_table_styles.push(n);
            }
        }
        Ok(Map {
            xfs,
            dxfs,
            table_styles,
        })
    }

    fn check_limits(&self) -> Result<(), Error> {
        let over = |what: &str, n: usize, limit: usize, largest: usize| {
            let limit = limit.max(largest);
            if n <= limit {
                return Ok(());
            }
            Err(Error::limit_exceeded(
                what,
                &thousands(n as u64),
                &thousands(limit as u64),
            ))
        };
        let [xfs, fonts] = self.largest;
        over(
            "The merged workbook's cell format count",
            self.xfs.len as usize,
            MAX_CELL_FORMATS,
            xfs,
        )?;
        over(
            "The merged workbook's font count",
            self.fonts.len as usize,
            MAX_FONTS,
            fonts,
        )
    }

    /// The finished style sheet, when it differs from the base's.
    pub fn finish(self) -> Option<Node> {
        let pools = [
            ("numFmts", self.new_formats),
            (self.fonts.list, self.fonts.added),
            (self.fills.list, self.fills.added),
            (self.borders.list, self.borders.added),
            (self.style_xfs.list, self.style_xfs.added),
            (self.xfs.list, self.xfs.added),
            ("cellStyles", self.new_named),
            (self.dxfs.list, self.dxfs.added),
            ("tableStyles", self.new_table_styles),
        ];
        if pools.iter().all(|p| p.1.is_empty()) {
            return None;
        }
        let mut sheet = self.base;
        sheet.attrs = self.root.attrs;
        for (list, added) in pools {
            if added.is_empty() {
                continue;
            }
            let before = &ORDER[..ORDER.iter().position(|l| *l == list).unwrap()];
            let list = sheet.child_or_insert(S, list, before);
            list.children.extend(added.into_iter().map(Child::Node));
            let count = list.nodes().count().to_string();
            list.set_attr("", "", "count", &count);
        }
        Some(sheet)
    }
}

/// A custom table style's form: what it looks like, whatever it is called.
fn table_style_form(style: &Node) -> String {
    let mut n = style.clone();
    n.attrs.retain(|a| a.local != "name");
    form(&n)
}
