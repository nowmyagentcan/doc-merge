//! From arguments to input files: folders and patterns expanded in place, in natural order.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, thousands};

/// The most files one run merges.
pub const MAX_FILES: usize = 500;

const EXTENSIONS: [&str; 7] = ["docx", "dotx", "pptx", "potx", "ppsx", "xlsx", "xltx"];
const OUTPUT_EXTENSIONS: [&str; 3] = ["docx", "pptx", "xlsx"];

/// The files `args` name, in order. A folder gives its top-level Office files, and an argument
/// that doesn't exist with `*` or `?` in its last component gives the ones in its folder that
/// match; any other path stays as it is, so a missing file is found when it is opened.
pub fn expand(args: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut files = vec![];
    for arg in args {
        let name = arg.file_name().unwrap_or_default().to_string_lossy();
        let found = if fs::metadata(arg).is_ok_and(|m| m.is_dir()) {
            listed(arg, |_| true)
        } else if fs::symlink_metadata(arg).is_err() && name.contains(['*', '?']) {
            let dir = arg.parent().filter(|d| !d.as_os_str().is_empty());
            let pattern: Vec<char> = name.to_lowercase().chars().collect();
            listed(dir.unwrap_or(Path::new(".")), |n| {
                matches(&pattern, &n.to_lowercase().chars().collect::<Vec<_>>())
            })
        } else {
            files.push(arg.clone());
            continue;
        };
        if found.is_empty() {
            return Err(Error::no_match(&arg.to_string_lossy()));
        }
        files.extend(found);
    }
    match files.len() {
        0 => Err(Error::no_input()),
        1 => Err(Error::need_two_files(1)),
        n if n > MAX_FILES => Err(Error::limit_exceeded(
            "The number of files",
            &thousands(n as u64),
            "500",
        )),
        _ => Ok(files),
    }
}

/// The Office files directly in `dir` whose names `keep` accepts, in natural order; symlinks
/// and subfolders are left out, and a folder that can't be read has none.
fn listed(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut names: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name())
        .filter(|n| {
            let n = n.to_string_lossy();
            wanted(&n) && keep(&n)
        })
        .collect();
    names.sort_by(|a, b| natural(&a.to_string_lossy(), &b.to_string_lossy()));
    names.into_iter().map(|n| dir.join(n)).collect()
}

/// An Office file's name, not hidden, not a lock file and not one of our own outputs.
fn wanted(name: &str) -> bool {
    let name = name.to_lowercase();
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    let ours = OUTPUT_EXTENSIONS.contains(&ext)
        && stem.strip_prefix("merged").is_some_and(|rest| {
            rest.is_empty()
                || rest
                    .strip_prefix('-')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        });
    EXTENSIONS.contains(&ext) && !name.starts_with('.') && !name.starts_with("~$") && !ours
}

/// Whether `name` fits `pattern`, where `*` stands for any run of characters and `?` for one.
fn matches(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    // The last `*` and where in the name it stopped, to go back to when the rest doesn't fit.
    let mut star = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((s, at)) => {
                    star = Some((s, at + 1));
                    (p, n) = (s + 1, at + 1);
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Compares names ignoring case, with runs of digits compared as numbers; names equal that way
/// compare as they are.
fn natural(a: &str, b: &str) -> Ordering {
    let (x, y) = (a.to_lowercase(), b.to_lowercase());
    let (x, y) = (x.as_bytes(), y.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        if x[i].is_ascii_digit() && y[j].is_ascii_digit() {
            let ((p, n), (q, m)) = (number(&x[i..]), number(&y[j..]));
            let by_value = p.len().cmp(&q.len()).then_with(|| p.cmp(q));
            if by_value.is_ne() {
                return by_value;
            }
            (i, j) = (i + n, j + m);
        } else if x[i] != y[j] {
            return x[i].cmp(&y[j]);
        } else {
            (i, j) = (i + 1, j + 1);
        }
    }
    (x.len() - i).cmp(&(y.len() - j)).then_with(|| a.cmp(b))
}

/// The digits at the start of `s` without leading zeros, and how many there were with them.
fn number(s: &[u8]) -> (&[u8], usize) {
    let len = s.iter().take_while(|c| c.is_ascii_digit()).count();
    let zeros = s[..len].iter().take_while(|c| **c == b'0').count();
    (&s[zeros..len], len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(test: &str, names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("doc-merge-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub.pptx")).unwrap();
        for n in names {
            fs::write(dir.join(n), b"x").unwrap();
        }
        dir
    }

    fn names(files: &[PathBuf]) -> Vec<String> {
        let name = |p: &PathBuf| p.file_name().unwrap().to_string_lossy().into_owned();
        files.iter().map(name).collect()
    }

    #[test]
    fn names_sort_as_people_read_them() {
        let mut list = [
            "deck10.pptx",
            "deck2.pptx",
            "Deck3.pptx",
            "deck02.pptx",
            "a.pptx",
            "A.pptx",
            "deck99999999999999999999999.pptx",
            "deck100000000000000000000000.pptx",
            "é2.pptx",
            "é10.pptx",
        ];
        list.sort_by(|a, b| natural(a, b));
        assert_eq!(
            list,
            [
                "A.pptx",
                "a.pptx",
                "deck02.pptx",
                "deck2.pptx",
                "Deck3.pptx",
                "deck10.pptx",
                "deck99999999999999999999999.pptx",
                "deck100000000000000000000000.pptx",
                "é2.pptx",
                "é10.pptx",
            ]
        );
    }

    #[test]
    fn patterns_take_any_run_and_any_one_character() {
        let fits = |p: &str, n: &str| {
            let chars = |s: &str| s.chars().collect::<Vec<_>>();
            matches(&chars(p), &chars(n))
        };
        for (p, n) in [
            ("*.pptx", "a.pptx"),
            ("a?c*", "abc"),
            ("*a*b", "xaxb"),
            ("*", ""),
            ("**", "é"),
        ] {
            assert!(fits(p, n), "{p} {n}");
        }
        for (p, n) in [("*.pptx", "a.docx"), ("a?", "a"), ("a", "ab"), ("b*", "ab")] {
            assert!(!fits(p, n), "{p} {n}");
        }
        // Linear in practice: this would take years with plain backtracking.
        assert!(!fits(&"*a".repeat(40), &"a".repeat(39)));
    }

    #[test]
    fn folders_keep_office_files_only() {
        let dir = folder(
            "folder",
            &[
                "deck10.PPTX",
                "deck2.pptx",
                "~$deck2.pptx",
                ".hidden.pptx",
                "merged.pptx",
                "Merged-12.pptx",
                "merged-x.pptx",
                "merged.potx",
                "notes.txt",
                "no-extension",
            ],
        );
        let found = expand(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(
            names(&found),
            ["deck2.pptx", "deck10.PPTX", "merged-x.pptx", "merged.potx"]
        );
        assert_eq!(found[0], dir.join("deck2.pptx"));
    }

    #[test]
    fn each_argument_expands_in_place() {
        let dir = folder("order", &["b.docx", "a1.docx", "a2.docx", "z.pptx"]);
        let args = [
            dir.join("z.pptx"),
            dir.join("A?.DOCX"),
            dir.join("missing.docx"),
            dir.join("b.docx"),
        ];
        assert_eq!(
            names(&expand(&args).unwrap()),
            ["z.pptx", "a1.docx", "a2.docx", "missing.docx", "b.docx"]
        );
    }

    #[test]
    fn nothing_to_merge_is_refused_by_name() {
        let dir = folder("none", &["a.docx", "b.txt"]);
        let empty = dir.join("sub.pptx");
        let pattern = dir.join("*.pptx");
        let e = expand(&[empty.clone(), dir.join("a.docx")]).unwrap_err();
        assert_eq!(e.code, "no_match");
        assert_eq!(
            e.message,
            format!("No Office files match '{}'.", empty.display())
        );
        assert_eq!(expand(&[pattern]).unwrap_err().code, "no_match");
        let lost = dir.join("gone").join("*.docx");
        assert_eq!(expand(&[lost]).unwrap_err().code, "no_match");
        assert_eq!(expand(&[]).unwrap_err().code, "no_input");
        let e = expand(&[dir.join("a.docx")]).unwrap_err();
        assert_eq!(
            (e.code, e.exit, e.message.as_str()),
            (
                "need_two_files",
                2,
                "Give at least two files to merge (1 found)."
            )
        );
        let e = expand(&[dir.join("*.docx")]).unwrap_err();
        assert_eq!(e.code, "need_two_files");
    }

    #[test]
    fn a_run_takes_500_files_and_the_same_file_twice() {
        let file = PathBuf::from("a.docx");
        assert_eq!(expand(&vec![file.clone(); 500]).unwrap().len(), 500);
        let e = expand(&vec![file; 501]).unwrap_err();
        assert_eq!(e.code, "limit_exceeded");
        assert_eq!(
            e.message,
            "The number of files is over the limit: 501 (at most 500)."
        );
    }
}
