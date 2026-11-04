//! Text output for people: the result on stdout, errors on stderr, names made safe for a
//! terminal; colour only on a terminal without `NO_COLOR`, ASCII marks on a Windows console
//! whose output code page isn't UTF-8.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use doc_merge::error::Error;
use doc_merge::package::Kind;
use doc_merge::{Input, Report};

pub const BOLD: &str = "1";
pub const DIM: &str = "2";
const RED: &str = "31";
const GREEN: &str = "32";
const YELLOW: &str = "33";

/// At most this many renames are printed.
const RENAMES_SHOWN: usize = 10;

/// How one stream shows text.
#[derive(Debug, Clone, Copy, Default)]
pub struct Look {
    /// A terminal that takes ANSI sequences.
    pub ansi: bool,
    pub colour: bool,
    pub ascii: bool,
}

impl Look {
    pub fn stdout() -> Self {
        Self::new(io::stdout().is_terminal(), console::STDOUT)
    }

    pub fn stderr() -> Self {
        Self::new(io::stderr().is_terminal(), console::STDERR)
    }

    fn new(terminal: bool, stream: console::Stream) -> Self {
        let ansi = terminal && console::enable_ansi(stream);
        Self {
            ansi,
            colour: ansi && env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
            ascii: terminal && !console::utf8(),
        }
    }

    /// `s` in the SGR style `sgr` when colour is on.
    pub fn paint(self, sgr: &str, s: &str) -> String {
        if self.colour {
            format!("\x1b[{sgr}m{s}\x1b[0m")
        } else {
            s.into()
        }
    }

    /// `unicode`, or `ascii` where the console can't show it.
    pub fn pick(self, unicode: &'static str, ascii: &'static str) -> &'static str {
        if self.ascii { ascii } else { unicode }
    }
}

#[cfg(windows)]
mod console {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleOutputCP, GetStdHandle,
        STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    pub type Stream = STD_HANDLE;
    pub const STDOUT: Stream = STD_OUTPUT_HANDLE;
    pub const STDERR: Stream = STD_ERROR_HANDLE;

    /// Turns on ANSI sequences for the console behind `stream`.
    pub fn enable_ansi(stream: Stream) -> bool {
        // SAFETY: plain console calls on a standard handle; `mode` outlives the call.
        unsafe {
            let handle = GetStdHandle(stream);
            let mut mode = 0;
            GetConsoleMode(handle, &mut mode) != 0
                && SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }

    pub fn utf8() -> bool {
        // SAFETY: no arguments; returns 0 without a console.
        unsafe { GetConsoleOutputCP() == 65001 }
    }
}

#[cfg(not(windows))]
mod console {
    pub type Stream = ();
    pub const STDOUT: Stream = ();
    pub const STDERR: Stream = ();

    pub fn enable_ansi(_: Stream) -> bool {
        true
    }

    pub fn utf8() -> bool {
        true
    }
}

/// A stream and how it shows text.
pub struct Out<W> {
    pub w: W,
    pub look: Look,
    /// Interactive mode: lines sit under the prompt, indented.
    pub prompted: bool,
}

impl<W: Write> Out<W> {
    pub fn new(w: W, look: Look) -> Self {
        Self {
            w,
            look,
            prompted: false,
        }
    }

    pub fn line(&mut self, s: &str) -> io::Result<()> {
        match s.is_empty() || !self.prompted {
            true => writeln!(self.w, "{s}"),
            false => writeln!(self.w, "  {s}"),
        }
    }
}

/// `s` with control and bidi characters as `<U+XXXX>`, so a name can't move the cursor or
/// reorder the line it is printed in.
pub fn safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push(c),
            '\0'..='\x1F'
            | '\x7F'..='\u{9F}'
            | '\u{61C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}' => out += &format!("<U+{:04X}>", c as u32),
            c => out.push(c),
        }
    }
    out
}

/// `✗ message`, and the hint indented on the next line.
pub fn error(err: &mut Out<impl Write>, e: &Error) -> io::Result<()> {
    let mark = err.look.paint(RED, err.look.pick("✗", "x"));
    err.line(&format!("{mark} {}", safe(&e.message)))?;
    match e.hint {
        Some(hint) => err.line(&format!("  {hint}")),
        None => Ok(()),
    }
}

/// `{n} {what}`, with an `s` unless it is one.
fn counted(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// What the merged file holds, as a count of its slides, sheets or sections.
fn contents(r: &Report) -> String {
    let what = match r.output.kind {
        Kind::Pptx => "slide",
        Kind::Xlsx => "sheet",
        _ => "section",
    };
    let of = |i: &Input| i.slides.or(i.sections).or(i.sheets.as_ref().map(Vec::len));
    counted(r.inputs.iter().filter_map(of).sum(), what)
}

/// A file size: `B`, whole `KB`, or `MB` with one decimal; 1 KB is 1,024 bytes.
fn size(bytes: u64) -> String {
    match bytes {
        ..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KB", bytes / 1024),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn time(ms: u64) -> String {
    match ms {
        ..1000 => format!("{ms} ms"),
        _ => format!("{:.1} s", ms as f64 / 1000.0),
    }
}

/// A path relative to the current folder when it is inside it.
fn shown(path: &Path) -> String {
    let cwd = env::current_dir().unwrap_or_default();
    let path = path.strip_prefix(cwd).unwrap_or(path);
    safe(&path.to_string_lossy())
}

/// What a merge did: the result line, the renames and the warnings.
pub fn report(out: &mut Out<impl Write>, r: &Report) -> io::Result<()> {
    let look = out.look;
    let dim = |s: &str| look.paint(DIM, s);
    out.line(&format!(
        "{} {}  {} {}  {}  {}",
        look.paint(GREEN, look.pick("✓", "ok")),
        look.paint(BOLD, &shown(&r.output.path)),
        contents(r),
        dim(&format!("from {} files", r.inputs.len())),
        dim(&size(r.output.bytes)),
        dim(&time(r.elapsed_ms)),
    ))?;
    for rename in r.renames.iter().take(RENAMES_SHOWN) {
        let input = &r.inputs[rename.input - 1].path;
        let name = input.file_name().unwrap_or_default().to_string_lossy();
        out.line(&format!(
            "  {}  {} {} {} {}  {}",
            dim("renamed"),
            rename.kind.replace('_', " "),
            safe(&rename.from),
            look.pick("→", "->"),
            safe(&rename.to),
            dim(&format!("({})", safe(&name))),
        ))?;
    }
    let more = r.renames.len().saturating_sub(RENAMES_SHOWN) + r.renames_omitted;
    if more > 0 {
        let dots = look.pick("…", "...");
        out.line(&format!(
            "  {}",
            dim(&format!("{dots} and {more} more (use --json to see all)"))
        ))?;
    }
    for w in r.warnings.list() {
        out.line(&format!("{} {}", look.paint(YELLOW, "!"), safe(&w.message)))?;
    }
    Ok(())
}

/// How long work runs before the spinner shows, and how long each frame stays.
const SPIN_AFTER: Duration = Duration::from_millis(300);
const FRAME: Duration = Duration::from_millis(80);

/// Runs `work`; when it takes a while and stderr is a terminal, `⠋ {label}` spins on stderr
/// meanwhile and is erased before this returns.
pub fn spin<T>(err: &Out<impl Write>, label: &str, work: impl FnOnce() -> T) -> T {
    if !err.look.ansi {
        return work();
    }
    let (stop, stopped) = mpsc::channel();
    let look = err.look;
    let indent = if err.prompted { "  " } else { "" };
    let line = format!("{indent}{{}} {label}");
    thread::scope(|s| {
        s.spawn(|| spinner(io::stderr(), look, &line, stopped));
        let result = work();
        drop(stop);
        result
    })
}

/// Draws `line` (its `{}` the frame) until `stopped` ends, then erases it.
fn spinner(mut w: impl Write, look: Look, line: &str, stopped: Receiver<()>) {
    let frames: &[&str] = if look.ascii {
        &["|", "/", "-", "\\"]
    } else {
        &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
    };
    let mut drawn = false;
    for (i, frame) in frames.iter().cycle().enumerate() {
        let wait = if i == 0 { SPIN_AFTER } else { FRAME };
        if stopped.recv_timeout(wait) != Err(RecvTimeoutError::Timeout) {
            break;
        }
        let text = look.paint(DIM, &line.replacen("{}", frame, 1));
        if write!(w, "\r{text}").and_then(|()| w.flush()).is_err() {
            return;
        }
        drawn = true;
    }
    if drawn {
        let _ = write!(w, "\r\x1b[K");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use doc_merge::error::{Warning, Warnings};
    use doc_merge::{Output, Rename};

    const COLOUR: Look = Look {
        ansi: true,
        colour: true,
        ascii: false,
    };
    const ASCII: Look = Look {
        ansi: true,
        colour: false,
        ascii: true,
    };

    fn input(name: &str) -> Input {
        Input {
            path: env::current_dir().unwrap().join(name),
            kind: Kind::Pptx,
            sections: None,
            output_sections: None,
            slides: Some(12),
            output_slides: Some([1, 12]),
            sheets: None,
        }
    }

    fn sample() -> Report {
        let mut warnings = Warnings::default();
        let scaled = Warning::slides_scaled(2, "old-4x3.pptx", "4:3", "16:9");
        warnings.push(scaled);
        Report {
            output: Output {
                path: env::current_dir().unwrap().join("all.pptx"),
                kind: Kind::Pptx,
                bytes: 2_516_583,
            },
            inputs: vec![input("deck-a.pptx"), input("old-4x3.pptx")],
            renames: vec![Rename {
                input: 2,
                kind: "custom_show",
                from: "Short".into(),
                to: "Short (2)".into(),
            }],
            renames_omitted: 0,
            warnings,
            elapsed_ms: 1234,
            steps: [("open", 0), ("plan", 0), ("write", 0)],
        }
    }

    fn printed(r: &Report, look: Look) -> String {
        let mut out = Out::new(vec![], look);
        report(&mut out, r).unwrap();
        String::from_utf8(out.w).unwrap()
    }

    #[test]
    fn the_result_reads_as_designed() {
        assert_eq!(
            printed(&sample(), Look::default()),
            "✓ all.pptx  24 slides from 2 files  2.4 MB  1.2 s\n  renamed  custom show Short → Short (2)  (old-4x3.pptx)\n! Slides from old-4x3.pptx (4:3) were scaled to fit the 16:9 slide size and centred.\n"
        );
    }

    #[test]
    fn a_terminal_gets_colour_and_a_console_without_utf_8_gets_ascii() {
        assert_eq!(
            printed(&sample(), COLOUR).lines().next().unwrap(),
            "\x1b[32m✓\x1b[0m \x1b[1mall.pptx\x1b[0m  24 slides \x1b[2mfrom 2 files\x1b[0m  \x1b[2m2.4 MB\x1b[0m  \x1b[2m1.2 s\x1b[0m"
        );
        assert_eq!(
            printed(&sample(), ASCII),
            "ok all.pptx  24 slides from 2 files  2.4 MB  1.2 s\n  renamed  custom show Short -> Short (2)  (old-4x3.pptx)\n! Slides from old-4x3.pptx (4:3) were scaled to fit the 16:9 slide size and centred.\n"
        );
        let mut err = Out::new(vec![], ASCII);
        error(&mut err, &Error::no_input()).unwrap();
        assert!(
            String::from_utf8(err.w)
                .unwrap()
                .starts_with("x No files given.\n")
        );
    }

    #[test]
    fn long_rename_lists_are_cut_and_counted() {
        let mut r = sample();
        r.renames = vec![r.renames[0].clone(); 12];
        r.renames_omitted = 3;
        let text = printed(&r, Look::default());
        assert_eq!(text.matches("  renamed  ").count(), 10);
        assert!(
            text.contains("\n  … and 5 more (use --json to see all)\n"),
            "{text}"
        );
        assert!(printed(&r, ASCII).contains("\n  ... and 5 more (use"));
        r.renames.truncate(10);
        r.renames_omitted = 0;
        assert!(!printed(&r, Look::default()).contains("more"));
    }

    #[test]
    fn counts_follow_the_family() {
        let mut r = sample();
        r.inputs[1].slides = Some(1);
        assert!(printed(&r, Look::default()).contains("  13 slides from 2 files"));
        r.output.kind = Kind::Docx;
        for i in &mut r.inputs {
            (i.slides, i.sections) = (None, Some(1));
        }
        assert!(printed(&r, Look::default()).contains("  2 sections from 2 files"));
        r.output.kind = Kind::Xlsx;
        r.inputs[0].sections = None;
        r.inputs[1].sections = None;
        r.inputs[0].sheets = Some(vec![]);
        r.inputs[1].sheets = Some(vec![]);
        assert!(printed(&r, Look::default()).contains("  0 sheets from 2 files"));
        assert_eq!(counted(1, "slide"), "1 slide");
    }

    #[test]
    fn sizes_and_times_never_split_or_round_up_a_unit() {
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1 KB");
        assert_eq!(size(1_048_575), "1023 KB");
        assert_eq!(size(1_048_576), "1.0 MB");
        assert_eq!(time(999), "999 ms");
        assert_eq!(time(1000), "1.0 s");
    }

    #[test]
    fn controls_and_bidi_marks_are_shown_not_sent() {
        assert_eq!(
            safe(
                "a\x1b[31m\tb\n\u{7F}\u{85}\u{9B}\u{61C}\u{200E}\u{200F}\u{202A}\u{202E}\u{2066}\u{2069}é✓"
            ),
            "a<U+001B>[31m\tb<U+000A><U+007F><U+0085><U+009B><U+061C><U+200E><U+200F><U+202A><U+202E><U+2066><U+2069>é✓"
        );
        let mut r = sample();
        r.renames[0].from = "a\u{202E}b".into();
        r.warnings = Warnings::default();
        r.warnings.push(Warning::theme_differs(2, "x\u{1b}.pptx"));
        let text = printed(&r, Look::default());
        assert!(text.contains("custom show a<U+202E>b → "), "{text}");
        assert!(text.contains("from x<U+001B>.pptx use"), "{text}");
        let mut err = Out::new(vec![], Look::default());
        error(&mut err, &Error::macro_enabled("x\u{9B}.pptm")).unwrap();
        let text = String::from_utf8(err.w).unwrap();
        assert!(text.starts_with("✗ 'x<U+009B>.pptm' contains"), "{text}");
    }

    #[test]
    fn the_spinner_waits_then_erases_its_line() {
        let run = |wait: Option<Duration>| {
            let (stop, stopped) = mpsc::channel::<()>();
            let mut drawn = vec![];
            thread::scope(|s| {
                s.spawn(|| spinner(&mut drawn, Look::default(), "{} Merging", stopped));
                if let Some(wait) = wait {
                    thread::sleep(wait);
                }
                drop(stop);
            });
            String::from_utf8(drawn).unwrap()
        };
        assert_eq!(run(None), "");
        let text = run(Some(SPIN_AFTER + FRAME * 3));
        assert!(text.starts_with("\r⠋ Merging"), "{text}");
        assert!(text.ends_with("\r\x1b[K"), "{text}");
    }
}
