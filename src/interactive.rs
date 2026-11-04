//! Interactive mode: a banner, then a prompt that merges the paths typed or dropped on each
//! line.

use std::io::{self, BufRead, Write};
use std::mem;
use std::path::{Path, PathBuf};

use doc_merge::error::Error;

use crate::text::{self, BOLD, DIM, Out, safe};

const SENTENCE: &str = "Merge PowerPoint, Excel or Word files.";
const HELP: [&str; 4] = [
    "Type or drop two or more files of one kind, or a folder, to merge them in that order.",
    "The merged file is saved beside the first one as merged.pptx, merged.xlsx or merged.docx.",
    "/help    show this",
    "/quit    leave (Ctrl+C works too)",
];

/// Shows the banner, then hands each line's paths to `merge` until `/quit`, the end of `input`
/// or a merge that says to stop.
pub fn run<O: Write, E: Write>(
    mut input: impl BufRead,
    out: &mut Out<O>,
    err: &mut Out<E>,
    mut merge: impl FnMut(Vec<PathBuf>, &mut Out<O>, &mut Out<E>) -> io::Result<bool>,
) -> io::Result<()> {
    banner(out)?;
    let prompt = out.look.paint(BOLD, out.look.pick("›", ">"));
    (out.prompted, err.prompted) = (true, true);
    let mut line = vec![];
    loop {
        write!(out.w, "{prompt} ")?;
        out.w.flush()?;
        line.clear();
        // Bytes, not read_line: a path that isn't UTF-8 is a file not found, never a stop.
        if input.read_until(b'\n', &mut line)? == 0 {
            return writeln!(out.w);
        }
        let words = split(&String::from_utf8_lossy(&line), cfg!(not(windows)));
        match words.as_slice() {
            [] => continue,
            [w] if w == "/quit" => return Ok(()),
            [w] if w == "/help" => {
                for help in HELP {
                    out.line(help)?;
                }
            }
            [w] if is_command(w) => {
                let message = format!("Unknown command '{}'.", safe(w));
                let e = Error {
                    hint: Some("Try /help."),
                    ..Error::usage(message)
                };
                text::error(err, &e)?;
            }
            _ => {
                if !merge(words.iter().map(PathBuf::from).collect(), out, err)? {
                    return Ok(());
                }
            }
        }
        writeln!(out.w)?;
    }
}

/// A word like `/x` that isn't an existing path.
fn is_command(word: &str) -> bool {
    word.strip_prefix('/')
        .is_some_and(|name| name.chars().all(|c| c.is_ascii_alphabetic()))
        && !Path::new(word).exists()
}

fn banner(out: &mut Out<impl Write>) -> io::Result<()> {
    let look = out.look;
    let version = env!("CARGO_PKG_VERSION");
    let name = format!("{} doc-merge", look.pick("✻", "*"));
    let rows = [
        (
            format!("{name} {version}"),
            format!("{} {version}", look.paint(BOLD, &name)),
        ),
        (format!("  {SENTENCE}"), format!("  {SENTENCE}")),
    ];
    let width = rows.iter().map(|(plain, _)| plain.chars().count()).max();
    let width = width.unwrap_or_default();
    let [top_left, top_right, bottom_left, bottom_right, across, side] = if look.ascii {
        ["+", "+", "+", "+", "-", "|"]
    } else {
        ["╭", "╮", "╰", "╯", "─", "│"]
    };
    let rule = across.repeat(width + 2);
    let side = look.paint(DIM, side);
    out.line(&look.paint(DIM, &format!("{top_left}{rule}{top_right}")))?;
    for (plain, styled) in rows {
        let pad = " ".repeat(width - plain.chars().count());
        out.line(&format!("{side} {styled}{pad} {side}"))?;
    }
    out.line(&look.paint(DIM, &format!("{bottom_left}{rule}{bottom_right}")))?;
    let dot = look.pick("·", "-");
    let hint = format!("  Drop files here or type paths {dot} /help {dot} /quit");
    out.line(&look.paint(DIM, &hint))?;
    out.line("")
}

/// The words of a line, split on spaces outside quotes. With `posix` as a POSIX shell reads a
/// dropped file (backslash escapes, single and double quotes), otherwise as Windows quotes
/// paths (double quotes only, backslashes kept).
fn split(line: &str, posix: bool) -> Vec<String> {
    let (mut words, mut word, mut quote, mut started) = (vec![], String::new(), None, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, c) if c.is_ascii_whitespace() => {
                if mem::take(&mut started) {
                    words.push(mem::take(&mut word));
                }
                continue;
            }
            (None, '\\') if posix => word.extend(chars.next()),
            (None, '"' | '\'') if posix || c == '"' => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') if posix => match chars.next() {
                Some(escaped @ ('"' | '\\' | '$' | '`')) => word.push(escaped),
                other => {
                    word.push('\\');
                    word.extend(other);
                }
            },
            _ => word.push(c),
        }
        started = true;
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Look;

    #[test]
    fn posix_lines_split_like_a_shell() {
        let split = |line| split(line, true);
        assert_eq!(
            split(r"/tmp/a\ b.pptx  c.pptx"),
            ["/tmp/a b.pptx", "c.pptx"]
        );
        assert_eq!(split("'/x y/it''s.pptx'"), ["/x y/its.pptx"]);
        assert_eq!(split(r#""a \"b\" \\c\d" e"#), [r#"a "b" \c\d"#, "e"]);
        assert_eq!(split(r#"'it'\''s' "#), ["it's"]);
        assert!(split(" \t\n").is_empty());
    }

    #[test]
    fn windows_lines_split_on_spaces_outside_double_quotes() {
        let split = |line| split(line, false);
        assert_eq!(
            split("\"C:\\a b\\c.pptx\" d.pptx\r\n"),
            [r"C:\a b\c.pptx", "d.pptx"]
        );
        assert_eq!(split(r"C:\x\it's.pptx"), [r"C:\x\it's.pptx"]);
        assert_eq!(split(r#"dir\"a b"\c.pptx"#), [r"dir\a b\c.pptx"]);
    }

    /// Runs interactive mode on `typed`, recording the paths of each line.
    fn session(typed: &[u8], look: Look) -> (String, String) {
        let (mut out, mut err) = (Out::new(vec![], look), Out::new(vec![], look));
        run(typed, &mut out, &mut err, |paths, out, _| {
            out.line(&format!("merge {paths:?}"))?;
            Ok(paths.len() < 3)
        })
        .unwrap();
        let text = |w: Vec<u8>| String::from_utf8(w).unwrap();
        (text(out.w), text(err.w))
    }

    #[test]
    fn a_session_merges_each_line_until_quit() {
        let typed = "\"a b.pptx\" c.pptx\n\n/help\n/x\n/quit\nlater.pptx\n";
        let (out, err) = session(typed.as_bytes(), Look::default());
        let screen = [
            "╭──────────────────────────────────────────╮",
            "│ ✻ doc-merge 0.1.0                        │",
            "│   Merge PowerPoint, Excel or Word files. │",
            "╰──────────────────────────────────────────╯",
            "  Drop files here or type paths · /help · /quit",
            "",
            "›   merge [\"a b.pptx\", \"c.pptx\"]",
            "",
            "› ›   Type or drop two or more files of one kind, or a folder, to merge them in that order.",
            "  The merged file is saved beside the first one as merged.pptx, merged.xlsx or merged.docx.",
            "  /help    show this",
            "  /quit    leave (Ctrl+C works too)",
            "",
            "› ",
            "› ",
        ];
        assert_eq!(out, screen.join("\n"));
        assert_eq!(err, "  ✗ Unknown command '/x'.\n    Try /help.\n");
    }

    #[test]
    fn the_end_of_input_leaves_and_an_ascii_console_gets_ascii() {
        let ascii = Look {
            ascii: true,
            ..Look::default()
        };
        let (out, _) = session(b"", ascii);
        let screen = [
            "+------------------------------------------+",
            "| * doc-merge 0.1.0                        |",
            "|   Merge PowerPoint, Excel or Word files. |",
            "+------------------------------------------+",
            "  Drop files here or type paths - /help - /quit",
            "",
            "> \n",
        ];
        assert_eq!(out, screen.join("\n"));
    }

    #[test]
    fn a_merge_can_end_the_session() {
        let (out, _) = session(b"a b c\nd e\n", Look::default());
        assert!(out.ends_with("merge [\"a\", \"b\", \"c\"]\n"), "{out}");
    }

    #[test]
    fn a_line_that_is_not_utf_8_is_a_path_and_no_stop() {
        let (out, _) = session(b"caf\xE9.pptx\n/quit\n", Look::default());
        assert!(out.contains("merge [\"caf\u{FFFD}.pptx\"]"), "{out}");
    }

    /// Takes `room` bytes, then fails as a closed pipe does.
    struct Closing {
        room: usize,
    }

    impl Write for Closing {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.room < b.len() {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.room -= b.len();
            Ok(b.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_stream_ends_the_session_with_its_error() {
        for room in [0, 100, 460] {
            let mut out = Out::new(Closing { room }, Look::default());
            let mut err = Out::new(vec![], Look::default());
            let typed: &[u8] = b"/help\n/help\n/help\n";
            let e = run(typed, &mut out, &mut err, |_, _, _| Ok(true)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::BrokenPipe, "{room}");
        }
    }
}
