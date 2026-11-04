//! Excel's A1 formula text: a tokenizer that keeps every byte, and a rewriter that follows
//! renamed sheets, names and tables and shifted external-link numbers. Everything else in a
//! formula passes through untouched.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Operators, separators, whitespace and numbers.
    Other,
    String,
    /// A sheet prefix with its `!`: `Data!`, `'My data'!`, `Jan:Mar!`, `[1]Data!`, `[1]!`.
    Sheets,
    /// The name before a `(`.
    Function,
    /// `_xlpm.x`: a LET or LAMBDA parameter.
    Parameter,
    Name,
    /// The name before a structured reference.
    Table,
    /// `[...]` after a table name, brackets included.
    Structured,
    /// `A1`, `$A$1`, `A:C`.
    Cell,
    Bool,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: Kind,
    pub text: &'a str,
}

const ERRORS: [&str; 18] = [
    "#NULL!",
    "#DIV/0!",
    "#VALUE!",
    "#REF!",
    "#NAME?",
    "#NUM!",
    "#N/A",
    "#GETTING_DATA",
    "#SPILL!",
    "#CALC!",
    "#FIELD!",
    "#UNKNOWN!",
    "#BLOCKED!",
    "#CONNECT!",
    "#BUSY!",
    "#PYTHON!",
    "#TIMEOUT!",
    "#EXTERNAL!",
];

/// Whether a byte can be part of a name, a cell reference or a number. Bytes of non-ASCII
/// characters all count, so tokens always end on a character boundary.
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b >= 0x80 || b"_.\\?$".contains(&b)
}

fn word_end(b: &[u8], mut at: usize) -> usize {
    while at < b.len() && is_word(b[at]) {
        at += 1;
    }
    at
}

/// The end of text quoted with `q` starting at `at`, where a doubled `q` stands for itself.
fn quoted_end(b: &[u8], mut at: usize, q: u8) -> Option<usize> {
    at += 1;
    loop {
        match b.get(at)? {
            c if *c != q => at += 1,
            _ if b.get(at + 1) == Some(&q) => at += 2,
            _ => return Some(at + 1),
        }
    }
}

/// The end of the bracketed text starting at `at`; `'` escapes the next character inside.
fn bracket_end(b: &[u8], mut at: usize) -> Option<usize> {
    let mut depth = 0usize;
    loop {
        match b.get(at)? {
            b'\'' => at += 1,
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
        at += 1;
        if depth == 0 {
            return Some(at);
        }
    }
}

/// `at` (the end of a word) past `:word!` or `!`, when a sheet prefix ends there.
fn prefix_end(b: &[u8], at: usize) -> Option<usize> {
    match b.get(at)? {
        b'!' => Some(at + 1),
        b':' => {
            let end = word_end(b, at + 1);
            (end > at + 1 && b.get(end) == Some(&b'!')).then_some(end + 1)
        }
        _ => None,
    }
}

fn is_column(w: &str) -> bool {
    let w = w.strip_prefix('$').unwrap_or(w);
    (1..=3).contains(&w.len()) && column_number(w).is_some()
}

/// `A` is 1; none past `XFD`.
fn column_number(letters: &str) -> Option<u32> {
    let mut n = 0;
    for c in letters.bytes() {
        if !c.is_ascii_alphabetic() {
            return None;
        }
        n = n * 26 + u32::from(c.to_ascii_uppercase() - b'A') + 1;
    }
    (1..=16_384).contains(&n).then_some(n)
}

fn is_cell(w: &str) -> bool {
    let w = w.strip_prefix('$').unwrap_or(w);
    let digits = w.find(|c: char| c.is_ascii_digit() || c == '$');
    let Some((letters, row)) = digits.map(|d| w.split_at(d)) else {
        return false;
    };
    let row = row.strip_prefix('$').unwrap_or(row);
    (1..=3).contains(&letters.len())
        && column_number(letters).is_some()
        && row.bytes().all(|c| c.is_ascii_digit())
        && row
            .parse::<u32>()
            .is_ok_and(|n| (1..=1_048_576).contains(&n))
}

/// A word and what follows it: the end of its token and its kind.
fn word(f: &str, start: usize) -> (usize, Kind) {
    let b = f.as_bytes();
    let end = word_end(b, start);
    let w = &f[start..end];
    if let Some(after) = prefix_end(b, end) {
        return (after, Kind::Sheets);
    }
    if b[start].is_ascii_digit() || b[start] == b'.' {
        return (end, Kind::Other);
    }
    if w.starts_with("_xlpm.") {
        return (end, Kind::Parameter);
    }
    match b.get(end) {
        Some(b'(') => return (end, Kind::Function),
        Some(b'[') => return (end, Kind::Table),
        Some(b':') if is_column(w) => {
            let last = word_end(b, end + 1);
            if is_column(&f[end + 1..last]) {
                return (last, Kind::Cell);
            }
        }
        _ => {}
    }
    if w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false") {
        (end, Kind::Bool)
    } else if is_cell(w) {
        (end, Kind::Cell)
    } else {
        (end, Kind::Name)
    }
}

/// Splits a formula into tokens whose texts join back to it; `None` when it can't be read.
pub fn tokens(f: &str) -> Option<Vec<Token<'_>>> {
    let b = f.as_bytes();
    let mut out = Vec::with_capacity(8);
    let mut at = 0;
    let mut open: Vec<u8> = vec![];
    while at < b.len() {
        let start = at;
        let kind = match b[at] {
            b'"' => {
                at = quoted_end(b, at, b'"')?;
                Kind::String
            }
            b'\'' => {
                at = quoted_end(b, at, b'\'')?;
                if b.get(at) != Some(&b'!') {
                    return None;
                }
                at += 1;
                Kind::Sheets
            }
            b'[' => {
                at = bracket_end(b, at)?;
                let end = word_end(b, at);
                match prefix_end(b, end) {
                    Some(after) => {
                        at = after;
                        Kind::Sheets
                    }
                    None => Kind::Structured,
                }
            }
            b'#' => {
                let rest = &f[at..];
                let known = ERRORS.iter().find(|e| {
                    rest.get(..e.len())
                        .is_some_and(|r| r.eq_ignore_ascii_case(e))
                });
                match known {
                    Some(e) => {
                        at += e.len();
                        Kind::Error
                    }
                    None => {
                        at += 1;
                        Kind::Other
                    }
                }
            }
            c @ (b'(' | b'{') => {
                open.push(c);
                at += 1;
                Kind::Other
            }
            c @ (b')' | b'}') => {
                if open.pop()? != if c == b')' { b'(' } else { b'{' } {
                    return None;
                }
                at += 1;
                Kind::Other
            }
            b']' => return None,
            c if is_word(c) => {
                let (end, kind) = word(f, at);
                at = end;
                kind
            }
            _ => {
                at += 1;
                Kind::Other
            }
        };
        out.push(Token {
            kind,
            text: &f[start..at],
        });
    }
    open.is_empty().then_some(out)
}

/// What changed in one workbook, for the formulas in it. Keys are in lower case.
#[derive(Debug, Default)]
pub struct Renames {
    pub sheets: HashMap<String, String>,
    /// Workbook-scope names and tables; a name called like a function has its key end in `(`.
    pub names: HashMap<String, String>,
    /// Added to every external-link number from 1 up.
    pub links: u32,
}

impl Renames {
    pub fn are_empty(&self) -> bool {
        self.sheets.is_empty() && self.names.is_empty() && self.links == 0
    }

    /// A sheet prefix as the output needs it, when that differs.
    fn prefix(&self, text: &str) -> Option<String> {
        let body = text.strip_suffix('!')?;
        let quoted = body.starts_with('\'');
        let inner: Cow<str> = if quoted {
            body[1..body.len() - 1].replace("''", "'").into()
        } else {
            body.into()
        };
        let (book, sheets) = match inner.strip_prefix('[') {
            Some(rest) => {
                let (book, sheets) = rest.split_once(']')?;
                (Some(book), sheets)
            }
            None => (None, inner.as_ref()),
        };
        let number = book.and_then(|b| b.parse::<u32>().ok());
        let shifted = number
            .filter(|n| *n > 0 && self.links > 0)
            .map(|n| format!("[{}]", n + self.links));
        let own = book.is_none() || number == Some(0);
        let mut renamed = false;
        let mut names = vec![];
        for s in sheets.split(':') {
            let new = self.sheets.get(&s.to_lowercase()).filter(|_| own);
            renamed |= new.is_some();
            names.push(new.map_or(s, String::as_str));
        }
        if !renamed && shifted.is_none() {
            return None;
        }
        let book = shifted.unwrap_or_else(|| book.map_or(String::new(), |b| format!("[{b}]")));
        let sheets = names.join(":");
        Some(if quoted || renamed {
            format!("'{book}{}'!", sheets.replace('\'', "''"))
        } else {
            format!("{book}{sheets}!")
        })
    }

    /// `f` with every reference to something renamed followed. `local` holds the sheet-scope
    /// names in scope where `f` stands, which hide workbook-scope names. `None` when `f` can't
    /// be read.
    pub fn rewrite<'f>(&self, f: &'f str, local: &HashSet<String>) -> Option<Cow<'f, str>> {
        let tokens = tokens(f)?;
        if self.are_empty() {
            return Some(Cow::Borrowed(f));
        }
        let mut out = String::with_capacity(f.len() + 8);
        let mut changed = false;
        // A name after a sheet prefix is that sheet's or another workbook's.
        let mut qualified = false;
        for t in tokens {
            let new = match t.kind {
                Kind::Sheets => self.prefix(t.text),
                Kind::Name | Kind::Table if !qualified && !self.names.is_empty() => {
                    let key = t.text.to_lowercase();
                    self.names
                        .get(&key)
                        .filter(|_| t.kind == Kind::Table || !local.contains(&key))
                        .cloned()
                }
                Kind::Function if !qualified && !self.names.is_empty() => {
                    let key = t.text.to_lowercase();
                    let hidden = local.contains(&key);
                    self.names
                        .get(&format!("{key}("))
                        .filter(|_| !hidden)
                        .cloned()
                }
                _ => None,
            };
            qualified = t.kind == Kind::Sheets && !t.text.starts_with("[0]!");
            changed |= new.is_some();
            out.push_str(new.as_deref().unwrap_or(t.text));
        }
        Some(if changed { out.into() } else { f.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(f: &str) -> Vec<(Kind, &str)> {
        let t = tokens(f).unwrap_or_else(|| panic!("{f}"));
        assert_eq!(t.iter().map(|t| t.text).collect::<String>(), f);
        t.iter()
            .filter(|t| t.kind != Kind::Other)
            .map(|t| (t.kind, t.text))
            .collect()
    }

    #[test]
    fn strings_and_quoted_sheets_keep_their_doubled_quotes() {
        assert_eq!(
            kinds(r#""a""b"&'it''s'!A1&"x!y""#),
            [
                (Kind::String, r#""a""b""#),
                (Kind::Sheets, "'it''s'!"),
                (Kind::Cell, "A1"),
                (Kind::String, r#""x!y""#),
            ]
        );
    }

    #[test]
    fn sheet_prefixes_of_every_shape() {
        assert_eq!(
            kinds("SUM(Jan:Mar!B2,[1]Data!A1,'[2]My data'!A1,'Jan x:Mar y'!A1,[1]!Total,[0]!Rate)"),
            [
                (Kind::Function, "SUM"),
                (Kind::Sheets, "Jan:Mar!"),
                (Kind::Cell, "B2"),
                (Kind::Sheets, "[1]Data!"),
                (Kind::Cell, "A1"),
                (Kind::Sheets, "'[2]My data'!"),
                (Kind::Cell, "A1"),
                (Kind::Sheets, "'Jan x:Mar y'!"),
                (Kind::Cell, "A1"),
                (Kind::Sheets, "[1]!"),
                (Kind::Name, "Total"),
                (Kind::Sheets, "[0]!"),
                (Kind::Name, "Rate"),
            ]
        );
    }

    #[test]
    fn functions_parameters_and_structured_references() {
        assert_eq!(
            kinds("_xlfn.LET(_xlpm.x,1,_xlpm.x+LOG10(Sales[Amount]))"),
            [
                (Kind::Function, "_xlfn.LET"),
                (Kind::Parameter, "_xlpm.x"),
                (Kind::Parameter, "_xlpm.x"),
                (Kind::Function, "LOG10"),
                (Kind::Table, "Sales"),
                (Kind::Structured, "[Amount]"),
            ]
        );
        assert_eq!(
            kinds("Sales[[#This Row],[Qty]]*Sales['[x']]"),
            [
                (Kind::Table, "Sales"),
                (Kind::Structured, "[[#This Row],[Qty]]"),
                (Kind::Table, "Sales"),
                (Kind::Structured, "['[x']]"),
            ]
        );
    }

    #[test]
    fn booleans_errors_cells_and_names() {
        assert_eq!(
            kinds("IF(TRUE,#N/A,false)+#REF!+Data!#REF!+A1#"),
            [
                (Kind::Function, "IF"),
                (Kind::Bool, "TRUE"),
                (Kind::Error, "#N/A"),
                (Kind::Bool, "false"),
                (Kind::Error, "#REF!"),
                (Kind::Sheets, "Data!"),
                (Kind::Error, "#REF!"),
                (Kind::Cell, "A1"),
            ]
        );
        assert_eq!(
            kinds("$A$1:B$2+A:C+$A:$C+1:3+XFD1048576+XFE1+A1048577+R1C1+RC+Total_2+A"),
            [
                (Kind::Cell, "$A$1"),
                (Kind::Cell, "B$2"),
                (Kind::Cell, "A:C"),
                (Kind::Cell, "$A:$C"),
                (Kind::Cell, "XFD1048576"),
                (Kind::Name, "XFE1"),
                (Kind::Name, "A1048577"),
                (Kind::Name, "R1C1"),
                (Kind::Name, "RC"),
                (Kind::Name, "Total_2"),
                (Kind::Name, "A"),
            ]
        );
        assert_eq!(kinds("{1,2;\"a\",TRUE}").len(), 2);
        assert_eq!(kinds("1.5E+3+.5").len(), 0);
        assert_eq!(kinds("Größe*2"), [(Kind::Name, "Größe")]);
    }

    #[test]
    fn unreadable_formulas_say_so() {
        for f in [
            "\"open", "'open", "'a'+1", "SUM(1", "1)", "{1", "(}", "Sales[A", "A]",
        ] {
            assert_eq!(tokens(f), None, "{f}");
        }
    }

    fn renames() -> Renames {
        let map = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        Renames {
            sheets: map(&[("summary", "Summary (2)"), ("it's", "It's (2)")]),
            names: map(&[
                ("total", "Total_2"),
                ("sales", "Sales_2"),
                ("calc(", "Calc_2"),
            ]),
            links: 1,
        }
    }

    fn rewritten(f: &str) -> String {
        renames().rewrite(f, &HashSet::new()).unwrap().into_owned()
    }

    #[test]
    fn renamed_sheets_are_followed_and_quoted() {
        assert_eq!(rewritten("Summary!B2"), "'Summary (2)'!B2");
        assert_eq!(rewritten("SUMMARY!B2"), "'Summary (2)'!B2");
        assert_eq!(rewritten("'it''s'!A1"), "'It''s (2)'!A1");
        assert_eq!(rewritten("'Summary'!A1"), "'Summary (2)'!A1");
        assert_eq!(rewritten("Detail!A1"), "Detail!A1");
        assert_eq!(
            rewritten("SUM(Summary:Detail!A1)"),
            "SUM('Summary (2):Detail'!A1)"
        );
        assert_eq!(
            rewritten("SUM('Jan:Summary'!A1)"),
            "SUM('Jan:Summary (2)'!A1)"
        );
    }

    #[test]
    fn names_and_tables_follow_unless_something_hides_them() {
        assert_eq!(rewritten("Total*2"), "Total_2*2");
        assert_eq!(rewritten("TOTAL*2"), "Total_2*2");
        assert_eq!(rewritten("SUM(Sales[Amount])"), "SUM(Sales_2[Amount])");
        assert_eq!(rewritten("ROWS(Sales)"), "ROWS(Sales_2)");
        assert_eq!(rewritten("\"Total\"&Total"), "\"Total\"&Total_2");
        assert_eq!(rewritten("Data!Total"), "Data!Total");
        assert_eq!(rewritten("[1]!Total"), "[2]!Total");
        assert_eq!(rewritten("[0]!Total"), "[0]!Total_2");
        assert_eq!(rewritten("_xlpm.total+Total()"), "_xlpm.total+Total()");
        assert_eq!(rewritten("Calc(1)+calc"), "Calc_2(1)+calc");
        let hidden: HashSet<String> = ["total".to_string()].into();
        let r = renames();
        assert_eq!(r.rewrite("Total*2", &hidden).unwrap(), "Total*2");
        assert_eq!(
            r.rewrite("Total+Sales[A]", &hidden).unwrap(),
            "Total+Sales_2[A]"
        );
    }

    #[test]
    fn external_numbers_shift_and_own_workbook_stays() {
        assert_eq!(rewritten("[1]Summary!$A$1"), "[2]Summary!$A$1");
        assert_eq!(rewritten("'[3]My data'!A1"), "'[4]My data'!A1");
        assert_eq!(rewritten("[0]Summary!A1"), "'[0]Summary (2)'!A1");
        assert_eq!(rewritten("[Book.xlsx]Summary!A1"), "[Book.xlsx]Summary!A1");
    }

    #[test]
    fn untouched_text_is_borrowed_byte_for_byte() {
        let f = " SUM( A1 ,\tDetail!B2 )\n+\"Total\" ";
        assert!(matches!(
            renames().rewrite(f, &HashSet::new()).unwrap(),
            Cow::Borrowed(s) if s == f
        ));
        assert_eq!(Renames::default().rewrite("'a", &HashSet::new()), None);
    }
}
