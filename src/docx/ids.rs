//! Ids and names in Word content: every id an appended document brings gets a new value from
//! the output's pools, a bookmark name the output has is renamed with whatever points to it, and
//! a paragraph id changes only when it would repeat in the part it lands in.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;

use super::W;
use crate::error::Error;
use crate::xml::{Attr, escape_text};

pub const W14: &str = "http://schemas.microsoft.com/office/word/2010/wordml";
const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";

/// The highest value Word takes for a drawing's id, used as the top of every pool.
const MAX_ID: i64 = 2_147_483_646;
/// The value Word gives `w14:textId` when it tracks nothing, on most paragraphs of a document.
const NO_TEXT_ID: u32 = 0x7777_7777;
/// Bookmark names longer than this are refused by Word.
const MAX_BOOKMARK: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pool {
    Bookmark,
    Revision,
    Permission,
    ContentControl,
    Drawing,
}

const POOLS: usize = 5;

/// Where an element keeps a pool's id.
pub struct Slot {
    pub pool: Pool,
    attr_ns: &'static str,
    attr: &'static str,
    /// Whether a start element and an end element share the id.
    paired: bool,
}

/// The id an element holds, if it holds one of the pools'.
pub fn slot(ns: &str, local: &str) -> Option<Slot> {
    if ns == WP && local == "docPr" {
        return Some(Slot {
            pool: Pool::Drawing,
            attr_ns: "",
            attr: "id",
            paired: false,
        });
    }
    if ns != W {
        return None;
    }
    let (pool, attr, paired) = match local {
        "bookmarkStart" | "bookmarkEnd" => (Pool::Bookmark, "id", true),
        "permStart" | "permEnd" => (Pool::Permission, "id", true),
        "id" => (Pool::ContentControl, "val", false),
        "commentRangeStart" | "commentRangeEnd" => return None,
        l if l.ends_with("RangeStart") || l.ends_with("RangeEnd") => (Pool::Revision, "id", true),
        "ins" | "del" | "moveFrom" | "moveTo" | "rPrChange" | "pPrChange" | "sectPrChange"
        | "tblPrChange" | "tblGridChange" | "trPrChange" | "tcPrChange" | "tblPrExChange"
        | "numberingChange" | "cellIns" | "cellDel" | "cellMerge" => (Pool::Revision, "id", false),
        _ => return None,
    };
    Some(Slot {
        pool,
        attr_ns: W,
        attr,
        paired,
    })
}

impl Slot {
    fn find<'a>(&self, attrs: &'a [Attr]) -> Option<&'a Attr> {
        attrs
            .iter()
            .find(|a| a.ns == self.attr_ns && a.local == self.attr)
    }
}

/// What content points to outside itself: style ids, list ids, and footnote, endnote and
/// comment ids, in this order.
const TARGETS: usize = 5;

/// Which ids exist, in the order of [`Tally`]'s references.
pub type Known = [HashSet<String>; TARGETS];

/// How many of each defect a [`Tally`] holds: per pool repeated or unpaired ids, then per kind
/// of target references to nothing.
pub type Defects = [usize; POOLS + TARGETS];

const DEFECTS: [&str; POOLS + TARGETS] = [
    "bookmark ids",
    "revision ids",
    "permission ids",
    "content control ids",
    "drawing ids",
    "style references",
    "list references",
    "footnote references",
    "endnote references",
    "comment references",
];

fn target_of(local: &str) -> Option<usize> {
    match local {
        "pStyle" | "rStyle" | "tblStyle" => Some(0),
        "numId" => Some(1),
        "footnoteReference" => Some(2),
        "endnoteReference" => Some(3),
        "commentReference" | "commentRangeStart" | "commentRangeEnd" => Some(4),
        _ => None,
    }
}

/// The ids and references of some content.
#[derive(Default)]
pub struct Tally {
    seen: [HashMap<i64, u32>; POOLS],
    /// Per paired id, starts less ends.
    open: [HashMap<i64, i32>; POOLS],
    targets: [HashSet<String>; TARGETS],
}

impl Tally {
    pub fn see(&mut self, ns: &str, local: &str, attrs: &[Attr]) {
        if let Some(s) = slot(ns, local)
            && let Some(id) = s.find(attrs).and_then(|a| a.value.parse().ok())
        {
            let end = local.ends_with("End");
            if !end {
                *self.seen[s.pool as usize].entry(id).or_default() += 1;
            }
            if s.paired {
                *self.open[s.pool as usize].entry(id).or_default() += if end { -1 } else { 1 };
            }
        }
        if ns == W
            && let Some(k) = target_of(local)
        {
            let key = if matches!(k, 0 | 1) { "val" } else { "id" };
            let value = attrs.iter().find(|a| a.ns == W && a.local == key);
            let value = value
                .map(|a| a.value.as_str())
                .filter(|v| k != 1 || *v != "0");
            self.targets[k].extend(value.map(String::from));
        }
    }

    pub fn add(&mut self, other: Tally) {
        for (mine, theirs) in self.seen.iter_mut().zip(other.seen) {
            for (id, n) in theirs {
                *mine.entry(id).or_default() += n;
            }
        }
        for (mine, theirs) in self.open.iter_mut().zip(other.open) {
            for (id, n) in theirs {
                *mine.entry(id).or_default() += n;
            }
        }
        for (mine, theirs) in self.targets.iter_mut().zip(other.targets) {
            mine.extend(theirs);
        }
    }

    pub fn defects(&self, known: &Known) -> Defects {
        let pools = self.seen.iter().zip(&self.open).map(|(seen, open)| {
            let repeats: u32 = seen.values().map(|n| n - 1).sum();
            let unpaired: u32 = open.values().map(|n| n.unsigned_abs()).sum();
            (repeats + unpaired) as usize
        });
        let targets = self.targets.iter().zip(known);
        let targets = targets.map(|(used, known)| used.difference(known).count());
        let mut out = [0; POOLS + TARGETS];
        for (slot, n) in out.iter_mut().zip(pools.chain(targets)) {
            *slot = n;
        }
        out
    }
}

/// The output's defects may only be those its inputs brought.
pub fn check(output: &Defects, inputs: &Defects) -> Result<(), Error> {
    let worse = (0..output.len()).find(|&k| output[k] > inputs[k]);
    match worse {
        Some(k) => Err(Error::internal(&format!(
            "the merged document has {} bad {}, its inputs {}",
            output[k], DEFECTS[k], inputs[k]
        ))),
        None => Ok(()),
    }
}

#[derive(Default)]
struct IdPool {
    taken: HashSet<i64>,
    next: i64,
}

impl IdPool {
    /// The value after the highest; past the top of the pool, the lowest free one.
    fn fresh(&mut self) -> i64 {
        loop {
            if !(1..=MAX_ID).contains(&self.next) {
                self.next = 1;
            }
            self.next += 1;
            if !self.taken.contains(&(self.next - 1)) {
                return self.next - 1;
            }
        }
    }
}

/// The output's id pools.
#[derive(Default)]
pub struct Ids {
    pools: [IdPool; POOLS],
}

impl Ids {
    /// Takes in the ids the base keeps.
    pub fn base(tally: &Tally) -> Self {
        let mut ids = Ids::default();
        for (pool, seen) in ids.pools.iter_mut().zip(&tally.seen) {
            pool.taken = seen.keys().copied().collect();
            pool.next = pool.taken.iter().max().map_or(1, |m| m + 1);
        }
        ids
    }
}

/// What a walk over one part remembers of its ids.
#[derive(Default)]
pub struct Local {
    /// New ids of start and end elements seen so far.
    pairs: HashMap<(Pool, String), String>,
    /// Ids of the bookmarks left out.
    left_out: HashSet<String>,
}

impl Local {
    /// Gives the element's id its new value. A start and an end keep sharing one.
    pub fn renumber(&mut self, ids: &mut Ids, slot: &Slot, attrs: &mut [Attr]) -> bool {
        let Some(a) = attrs
            .iter_mut()
            .find(|a| a.ns == slot.attr_ns && a.local == slot.attr)
        else {
            return false;
        };
        let pool = &mut ids.pools[slot.pool as usize];
        a.value = if slot.paired {
            let key = (slot.pool, a.value.clone());
            self.pairs
                .entry(key)
                .or_insert_with(|| pool.fresh().to_string())
                .clone()
        } else {
            pool.fresh().to_string()
        };
        true
    }

    /// Whether a bookmark's start or end stays out of the output: every one when `all`, and
    /// Word's hidden last-edit mark.
    pub fn leaves_out(&mut self, local: &str, attrs: &[Attr], all: bool) -> bool {
        let value = |name: &str| {
            attrs
                .iter()
                .find(|a| a.ns == W && a.local == name)
                .map(|a| a.value.as_str())
        };
        let id = value("id").unwrap_or_default();
        if local == "bookmarkEnd" {
            return self.left_out.remove(id);
        }
        let hidden = value("name").is_some_and(|n| n.eq_ignore_ascii_case(HIDDEN_BOOKMARK));
        if all || hidden {
            self.left_out.insert(id.to_string());
        }
        all || hidden
    }
}

/// Word's mark of the last edit, which an appended document doesn't bring.
pub const HIDDEN_BOOKMARK: &str = "_GoBack";

/// Settles an appended document's bookmark names against the output's `taken` (lower case).
/// Returns the renames by lower-case name, and each as (from, to) in order.
pub fn rename_bookmarks(
    names: &[String],
    position: usize,
    taken: &mut HashSet<String>,
) -> (HashMap<String, String>, Vec<(String, String)>) {
    let mut own = HashSet::new();
    let mut clashes = vec![];
    for name in names {
        let lower = name.to_lowercase();
        if own.insert(lower.clone()) && !taken.insert(lower) {
            clashes.push(name);
        }
    }
    let mut map = HashMap::new();
    let mut listed = vec![];
    for name in clashes {
        let new = (position..)
            .map(|k| {
                let suffix = format!("_{k}");
                let keep = MAX_BOOKMARK.saturating_sub(suffix.len());
                let stem: String = name.chars().take(keep).collect();
                stem + &suffix
            })
            .find(|new| taken.insert(new.to_lowercase()))
            .expect("a free name");
        map.insert(name.to_lowercase(), new.clone());
        listed.push((name.clone(), new));
    }
    (map, listed)
}

/// Where the arguments of a field instruction are, without their quotes.
fn tokens(s: &str) -> Vec<Range<usize>> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if b[i] == b'"' {
            let from = i + 1;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = i.min(b.len());
            out.push(from..i);
            i += 1;
        } else {
            let from = i;
            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'"' {
                i += 1;
            }
            out.push(from..i);
        }
    }
    out
}

/// Field names that a bare `{ name }` field of a bookmark can't be.
const FIELDS: [&str; 76] = [
    "ADDRESSBLOCK",
    "ADVANCE",
    "AUTHOR",
    "AUTONUM",
    "AUTONUMLGL",
    "AUTONUMOUT",
    "AUTOTEXT",
    "AUTOTEXTLIST",
    "BARCODE",
    "BIBLIOGRAPHY",
    "BIDIOUTLINE",
    "CITATION",
    "COMMENTS",
    "COMPARE",
    "CREATEDATE",
    "DATABASE",
    "DATE",
    "DISPLAYBARCODE",
    "DOCPROPERTY",
    "DOCVARIABLE",
    "EDITTIME",
    "EMBED",
    "EQ",
    "FILENAME",
    "FILESIZE",
    "FILLIN",
    "FORMCHECKBOX",
    "FORMDROPDOWN",
    "FORMTEXT",
    "GREETINGLINE",
    "IF",
    "INCLUDEPICTURE",
    "INCLUDETEXT",
    "INDEX",
    "INFO",
    "KEYWORDS",
    "LASTSAVEDBY",
    "LINK",
    "LISTNUM",
    "MACROBUTTON",
    "MERGEBARCODE",
    "MERGEFIELD",
    "MERGEREC",
    "MERGESEQ",
    "NEXT",
    "NEXTIF",
    "NUMCHARS",
    "NUMPAGES",
    "NUMWORDS",
    "PAGE",
    "PRINT",
    "PRINTDATE",
    "PRIVATE",
    "QUOTE",
    "RD",
    "REVNUM",
    "SAVEDATE",
    "SECTION",
    "SECTIONPAGES",
    "SEQ",
    "SHAPE",
    "SKIPIF",
    "STYLEREF",
    "SUBJECT",
    "SYMBOL",
    "TA",
    "TC",
    "TEMPLATE",
    "TIME",
    "TITLE",
    "TOA",
    "TOC",
    "USERADDRESS",
    "USERINITIALS",
    "USERNAME",
    "XE",
];

/// A field instruction with its bookmark argument renamed through `names`, when it has one.
pub fn rename_in_field(instr: &str, names: &HashMap<String, String>) -> Option<String> {
    let t = tokens(instr);
    let name = instr[t.first()?.clone()].to_ascii_uppercase();
    let at = match name.as_str() {
        "REF" | "PAGEREF" | "NOTEREF" | "GOTOBUTTON" | "SET" | "ASK" => t.get(1)?,
        "HYPERLINK" => {
            let switch = |w: &&[Range<usize>]| instr[w[0].clone()].eq_ignore_ascii_case("\\l");
            &t.windows(2).find(switch)?[1]
        }
        n if FIELDS.contains(&n) => return None,
        _ => &t[0],
    };
    let new = names.get(&instr[at.clone()].to_lowercase())?;
    Some(format!("{}{new}{}", &instr[..at.start], &instr[at.end..]))
}

/// The pieces of the field instruction being read, as ranges of the output buffer: Word can
/// split one over several `w:instrText`.
#[derive(Default)]
pub struct Field {
    open: Option<usize>,
    pieces: Vec<Range<usize>>,
}

impl Field {
    pub fn open(&mut self, at: usize) {
        self.open = Some(at);
    }

    pub fn close(&mut self, at: usize) {
        if let Some(from) = self.open.take() {
            self.pieces.push(from..at);
        }
    }

    /// Renames the bookmark of the instruction read so far, right in `buf`.
    pub fn settle(&mut self, names: &HashMap<String, String>, buf: &mut Vec<u8>) {
        let pieces = std::mem::take(&mut self.pieces);
        if names.is_empty() || pieces.is_empty() {
            return;
        }
        let mut instr = String::new();
        for p in &pieces {
            let raw = String::from_utf8_lossy(&buf[p.clone()]);
            instr += &quick_xml::escape::unescape(&raw).unwrap_or(raw.clone());
        }
        let Some(new) = rename_in_field(&instr, names) else {
            return;
        };
        for p in pieces[1..].iter().rev() {
            buf.drain(p.clone());
        }
        let first = pieces[0].clone();
        buf.splice(first, escape_text(&new).bytes());
    }
}

/// `w14:paraId` or `w14:textId` as a number, unless it is the value that means nothing.
pub fn paragraph_id(a: &Attr) -> Option<u32> {
    if a.ns != W14 || !matches!(a.local.as_str(), "paraId" | "textId") {
        return None;
    }
    u32::from_str_radix(&a.value, 16)
        .ok()
        .filter(|&v| v != NO_TEXT_ID)
}

/// New values for those of `wanted` that are in `taken`, the rest joining `taken`: the first
/// free of `hash(input, old, attempt)`, above 0 and below 0x80000000.
pub fn new_values(
    input: usize,
    wanted: &BTreeSet<u32>,
    taken: &mut HashSet<u32>,
) -> HashMap<u32, u32> {
    let clashes: Vec<u32> = wanted
        .iter()
        .copied()
        .filter(|&id| !taken.insert(id))
        .collect();
    let mut map = HashMap::new();
    for old in clashes {
        let new = (0u32..).find_map(|attempt| {
            let mut h = DefaultHasher::new();
            (input, old, attempt).hash(&mut h);
            let value = h.finish() as u32 & 0x7FFF_FFFF;
            (value > 0 && taken.insert(value)).then_some(value)
        });
        map.insert(old, new.expect("a free value"));
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn fields_name_their_bookmark_in_their_own_place() {
        let map = names(&[("intro", "Intro_2"), ("page", "Page_2")]);
        for (from, to) in [
            (" REF Intro \\h ", Some(" REF Intro_2 \\h ")),
            ("ref INTRO", Some("ref Intro_2")),
            (" PAGEREF \"Intro\" \\p", Some(" PAGEREF \"Intro_2\" \\p")),
            (" NOTEREF Intro", Some(" NOTEREF Intro_2")),
            (
                " GOTOBUTTON Intro click here",
                Some(" GOTOBUTTON Intro_2 click here"),
            ),
            (" SET Intro \"x\"", Some(" SET Intro_2 \"x\"")),
            (" ASK Intro \"Why?\"", Some(" ASK Intro_2 \"Why?\"")),
            (
                " HYPERLINK \\l \"Intro\" ",
                Some(" HYPERLINK \\l \"Intro_2\" "),
            ),
            (
                " HYPERLINK \"http://x\" \\l \"Intro\"",
                Some(" HYPERLINK \"http://x\" \\l \"Intro_2\""),
            ),
            (" HYPERLINK \"Intro\"", None),
            (" Intro \\* MERGEFORMAT ", Some(" Intro_2 \\* MERGEFORMAT ")),
            (" PAGE ", None),
            (" REF Other ", None),
            (" REF", None),
            ("", None),
            (" TOC \\o \"1-3\" ", None),
        ] {
            assert_eq!(rename_in_field(from, &map).as_deref(), to, "{from}");
        }
    }

    #[test]
    fn a_split_instruction_is_renamed_as_one() {
        let map = names(&[("intro", "Intro_2")]);
        let mut buf = b"<t> REF In</t><x/><t>tro \\h</t>".to_vec();
        let mut field = Field::default();
        field.open(3);
        field.close(10);
        field.open(21);
        field.close(27);
        field.settle(&map, &mut buf);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "<t> REF Intro_2 \\h</t><x/><t></t>"
        );
    }

    #[test]
    fn clashing_names_take_the_lowest_free_number_within_forty_characters() {
        let mut taken: HashSet<String> = ["intro", "intro_2", "x".repeat(40).as_str()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let list = ["INTRO".to_string(), "x".repeat(40), "Own".to_string()];
        let (map, listed) = rename_bookmarks(&list, 2, &mut taken);
        assert_eq!(map["intro"], "INTRO_3");
        let cut = &map[&"x".repeat(40)];
        assert_eq!(cut.len(), 40);
        assert!(cut.ends_with("_2"));
        assert_eq!(listed.len(), 2);
        assert!(!map.contains_key("own") && taken.contains("own"));
    }

    #[test]
    fn a_name_an_appended_document_also_has_isnt_taken_by_a_rename() {
        let mut taken: HashSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        let list = ["a".to_string(), "a_2".to_string()];
        let (map, _) = rename_bookmarks(&list, 2, &mut taken);
        assert_eq!(map["a"], "a_3");
    }

    #[test]
    fn ids_past_the_top_start_again_at_the_lowest_free() {
        let mut tally = Tally::default();
        for id in ["2147483646", "1", "2"] {
            let attrs = [Attr::new("id", "", id)];
            tally.see(WP, "docPr", &attrs);
        }
        let mut ids = Ids::base(&tally);
        let mut local = Local::default();
        let s = slot(WP, "docPr").unwrap();
        let mut attrs = [Attr::new("id", "", "9")];
        local.renumber(&mut ids, &s, &mut attrs);
        assert_eq!(attrs[0].value, "3");
    }

    #[test]
    fn a_start_and_its_end_stay_a_pair_and_other_ids_never_repeat() {
        let mut ids = Ids::default();
        let mut local = Local::default();
        let mut new = |ns: &str, element: &str, key: &str, value: &str| {
            let mut attrs = [Attr::new(&format!("w:{key}"), ns, value)];
            let s = slot(ns, element).unwrap();
            local.renumber(&mut ids, &s, &mut attrs);
            attrs[0].value.clone()
        };
        let start = new(W, "bookmarkStart", "id", "5");
        let other = new(W, "bookmarkStart", "id", "6");
        assert_eq!(new(W, "bookmarkEnd", "id", "5"), start);
        assert_ne!(start, other);
        let moved = new(W, "moveFromRangeStart", "id", "5");
        assert_eq!(new(W, "moveFromRangeEnd", "id", "5"), moved);
        assert_ne!(new(W, "ins", "id", "7"), new(W, "ins", "id", "7"));
    }

    fn tally(elements: &[(&str, &str, &str)]) -> Tally {
        let mut t = Tally::default();
        for (element, key, value) in elements {
            t.see(W, element, &[Attr::new(&format!("w:{key}"), W, value)]);
        }
        t
    }

    #[test]
    fn repeats_lone_ends_and_dangling_references_count_as_defects() {
        let mut known = Known::default();
        known[0].insert("Normal".into());
        known[4].insert("7".into());
        let fine = tally(&[
            ("bookmarkStart", "id", "1"),
            ("bookmarkEnd", "id", "1"),
            ("id", "val", "5"),
            ("ins", "id", "9"),
            ("pStyle", "val", "Normal"),
            ("numId", "val", "0"),
            ("commentReference", "id", "7"),
        ]);
        assert_eq!(fine.defects(&known), [0; 10]);
        let bad = tally(&[
            ("bookmarkStart", "id", "1"),
            ("bookmarkStart", "id", "1"),
            ("bookmarkEnd", "id", "1"),
            ("permEnd", "id", "2"),
            ("id", "val", "5"),
            ("id", "val", "5"),
            ("pStyle", "val", "Missing"),
            ("numId", "val", "3"),
            ("footnoteReference", "id", "1"),
            ("endnoteReference", "id", "1"),
            ("commentRangeStart", "id", "8"),
        ]);
        assert_eq!(bad.defects(&known), [2, 0, 1, 1, 0, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn only_defects_the_inputs_lacked_fail_the_check() {
        let mut inputs = [0; 10];
        inputs[1] = 2;
        let mut output = inputs;
        assert!(check(&output, &inputs).is_ok());
        output[1] = 1;
        assert!(check(&output, &inputs).is_ok());
        output[0] = 1;
        let e = check(&output, &inputs).unwrap_err();
        assert_eq!(e.code, "internal");
        assert!(
            e.message.contains("1 bad bookmark ids, its inputs 0"),
            "{}",
            e.message
        );
    }

    #[test]
    fn comment_ranges_are_no_pool_of_this_kind() {
        assert!(slot(W, "commentRangeStart").is_none());
        assert!(slot("urn:other", "ins").is_none());
    }

    #[test]
    fn only_a_repeated_paragraph_id_changes() {
        let mut taken: HashSet<u32> = [0x1234, 0x5678].into_iter().collect();
        let wanted: BTreeSet<u32> = [0x1234, 0x9999].into_iter().collect();
        let map = new_values(2, &wanted, &mut taken);
        assert_eq!(map.len(), 1);
        let new = map[&0x1234];
        assert!(new > 0 && new <= 0x7FFF_FFFF && new != 0x1234);
        assert!(taken.contains(&new) && taken.contains(&0x9999));
        let again = new_values(2, &wanted, &mut taken.clone());
        assert_eq!(again.len(), 2, "both repeat now");
    }

    #[test]
    fn the_value_for_no_text_id_is_never_compared() {
        let id = |key: &str, v: &str| paragraph_id(&Attr::new(&format!("w14:{key}"), W14, v));
        assert_eq!(id("textId", "77777777"), None);
        assert_eq!(id("textId", "0000000A"), Some(10));
        assert_eq!(id("paraId", "1A"), Some(26));
        assert_eq!(paragraph_id(&Attr::new("w:paraId", W, "1A")), None);
    }
}
