//! From arguments to input files, with folders and patterns expanded in place in natural
//! order, and from `-o` to where the merged file goes.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, thousands};
use crate::package::Family;

/// The most files one run merges.
pub const MAX_FILES: usize = 500;

const EXTENSIONS: [&str; 7] = ["docx", "dotx", "pptx", "potx", "ppsx", "xlsx", "xltx"];
const OUTPUT_EXTENSIONS: [&str; 3] = ["docx", "pptx", "xlsx"];

/// What was asked of the output: `-o` and `--overwrite`.
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub output: Option<PathBuf>,
    pub overwrite: bool,
}

/// Where a merge may write, decided before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// This path; a file there is replaced only with `replace`.
    File { path: PathBuf, replace: bool },
    /// The first free name of `merged.<ext>`, `merged-2.<ext>` … `merged-999.<ext>` in `dir`.
    Free { dir: PathBuf, ext: &'static str },
}

impl Place {
    pub fn dir(&self) -> &Path {
        match self {
            Place::File { path, .. } => folder_of(path),
            Place::Free { dir, .. } => dir,
        }
    }

    pub fn replaces(&self) -> bool {
        matches!(self, Place::File { replace: true, .. })
    }

    /// The paths to try, in order.
    pub fn paths(&self) -> Vec<PathBuf> {
        match self {
            Place::File { path, .. } => vec![path.clone()],
            Place::Free { dir, ext } => std::iter::once(format!("merged.{ext}"))
                .chain((2..=999).map(|n| format!("merged-{n}.{ext}")))
                .map(|name| dir.join(name))
                .collect(),
        }
    }
}

fn folder_of(path: &Path) -> &Path {
    path.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Whether two paths are the same file; a path that doesn't exist is no file.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (fs::metadata(a), fs::metadata(b)) {
            return (a.dev(), a.ino()) == (b.dev(), b.ino());
        }
    }
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

impl Target {
    /// Where merging `inputs` into a file of `family` writes; no folder is looked at beyond
    /// what the checks need, so a free name is found only when the file is ready.
    pub fn place(&self, inputs: &[PathBuf], family: Family) -> Result<Place, Error> {
        let ext = family.extension();
        let Some(output) = &self.output else {
            let dir = folder_of(&inputs[0]).to_path_buf();
            return Ok(Place::Free { dir, ext });
        };
        let shown = output.to_string_lossy();
        if output.is_dir() || shown.ends_with(['/', '\\']) {
            let dir = match cfg!(windows) {
                true => output.clone(),
                false => PathBuf::from(shown.trim_end_matches('\\')),
            };
            return Ok(Place::Free { dir, ext });
        }
        if !output
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            let stem = output.file_stem().unwrap_or_default().to_string_lossy();
            return Err(Error::bad_extension(ext, &stem));
        }
        if inputs.iter().any(|i| same_file(i, output)) {
            return Err(Error::output_is_input(&shown));
        }
        if !self.overwrite && fs::symlink_metadata(output).is_ok() {
            return Err(Error::output_exists(&shown));
        }
        Ok(Place::File {
            path: output.clone(),
            replace: self.overwrite,
        })
    }
}

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

    fn place(output: Option<&Path>, overwrite: bool, inputs: &[PathBuf]) -> Result<Place, Error> {
        let target = Target {
            output: output.map(Path::to_path_buf),
            overwrite,
        };
        target.place(inputs, Family::PowerPoint)
    }

    #[test]
    fn without_o_the_file_goes_beside_the_first_input() {
        let free = |dir: &str| Place::Free {
            dir: dir.into(),
            ext: "pptx",
        };
        let input = Path::new("work").join("a.pptx");
        assert_eq!(place(None, false, &[input]), Ok(free("work")));
        let input = PathBuf::from("a.pptx");
        let place = place(None, true, &[input]).unwrap();
        assert_eq!(place, free("."));
        assert!(!place.replaces());
        let names = place.paths();
        assert_eq!(
            (names.len(), names[0].clone(), names[1].clone()),
            (
                999,
                Path::new(".").join("merged.pptx"),
                Path::new(".").join("merged-2.pptx")
            )
        );
        assert_eq!(names[998], Path::new(".").join("merged-999.pptx"));
    }

    #[test]
    fn o_names_a_folder_or_a_file_of_the_right_kind() {
        let dir = folder("target", &["a.pptx", "taken.pptx"]);
        let free = |dir: &Path| Place::Free {
            dir: dir.into(),
            ext: "pptx",
        };
        let inputs = [dir.join("a.pptx")];
        assert_eq!(place(Some(&dir), true, &inputs), Ok(free(&dir)));
        // A folder whose name looks like a file is still a folder.
        let named = dir.join("sub.pptx");
        assert_eq!(place(Some(&named), false, &inputs), Ok(free(&named)));
        let new = PathBuf::from(format!("{}/new/", dir.display()));
        assert_eq!(place(Some(&new), false, &inputs), Ok(free(&new)));

        let path = dir.join("out.PPTX");
        let replace = false;
        assert_eq!(
            place(Some(&path), false, &inputs),
            Ok(Place::File { path, replace })
        );
        for (name, message) in [
            ("all.docx", "The output is a .pptx file; name it all.pptx."),
            ("all.potx", "The output is a .pptx file; name it all.pptx."),
            ("all", "The output is a .pptx file; name it all.pptx."),
        ] {
            let e = place(Some(&dir.join(name)), true, &inputs).unwrap_err();
            assert_eq!((e.code, e.exit), ("bad_extension", 2));
            assert_eq!(e.message, message);
        }
    }

    #[test]
    fn a_taken_output_needs_overwrite_and_an_input_never_is_one() {
        let dir = folder("taken", &["a.pptx", "taken.pptx"]);
        let inputs = [dir.join("a.pptx")];
        let path = dir.join("taken.pptx");
        let e = place(Some(&path), false, &inputs).unwrap_err();
        assert_eq!((e.code, e.exit), ("output_exists", 1));
        assert_eq!(e.message, format!("{} already exists.", path.display()));
        let replace = true;
        assert_eq!(
            place(Some(&path), true, &inputs),
            Ok(Place::File {
                path: path.clone(),
                replace
            })
        );
        for overwrite in [false, true] {
            // The same file by another path.
            let other = dir.join("sub.pptx").join("..").join("a.pptx");
            let e = place(Some(&other), overwrite, &inputs).unwrap_err();
            assert_eq!((e.code, e.exit), ("output_is_input", 1));
        }
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
