//! The command line: `doc-merge [merge] <PATH>... [OPTIONS]`.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use doc_merge::error::Error;
use doc_merge::paths::{self, Target};
use doc_merge::{Options, Report, STOP, StyleMode, guarded, merge};
use serde::Serialize;

/// `doc-merge ui`
#[derive(Parser)]
#[command(
    name = "doc-merge ui",
    about = "Open the local page in your browser.",
    disable_version_flag = true
)]
struct Ui {
    /// The port on 127.0.0.1; leave it out for a free one
    #[arg(long, value_name = "N")]
    port: Option<u16>,
    /// Don't open the browser
    #[arg(long)]
    no_open: bool,
}

/// `doc-merge mcp`
#[derive(Parser)]
#[command(
    name = "doc-merge mcp",
    about = "Run the MCP server on stdio.",
    disable_version_flag = true
)]
struct Mcp {}

#[derive(Parser, Default)]
#[command(
    name = "doc-merge",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {
    paths: Vec<PathBuf>,
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    #[arg(long, value_name = "MODE", value_parser = ["keep", "first"])]
    styles: Option<String>,
    #[arg(long)]
    overwrite: bool,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    debug: bool,
    #[arg(short, long)]
    help: bool,
    #[arg(short = 'V', long)]
    version: bool,
}

const HELP: &str = "\
Merge PowerPoint, Excel or Word files.

Usage: doc-merge [merge] <PATH>... [OPTIONS]
       doc-merge ui [--port N] [--no-open]
       doc-merge mcp

Files are merged in the order given; folders and * patterns expand in name order.
The first file is the base: its theme, page setup and properties carry over.

Commands:
  merge  Merge two or more files of one kind (the default when the first argument is a path)
  ui     Open the local page in your browser
  mcp    Run the MCP server on stdio

Options:
  -o, --output PATH   Output file or folder (default: merged.<ext> beside the first file)
      --styles MODE   Word: keep (each document's look, the default) or first (the first document's styles)
      --overwrite     Replace an existing -o file
      --json          Print one JSON object
      --debug         Show cause chains and timings
      --port N        ui: port to listen on (default: a free one)
      --no-open       ui: don't open the browser
  -h, --help          Print help
  -V, --version       Print version

What each kind keeps:
  PowerPoint  every slide with its master, layouts and theme, notes, charts, comments,
              sections, custom shows and slide links; slides of another size are scaled to fit
  Excel       every sheet with its formats, tables, charts, pictures, comments and names;
              clashing sheet, table and name references are renamed and formulas rewritten
  Word        each document from a new page with its page setup, headers, footers, lists,
              notes and comments; clashing styles keep each document's look (or --styles first)

Refused: macros, password-protected and old binary files, Strict Open XML, ODF, .xlsb;
in Excel files after the first, pivot tables, slicers, timelines, data models, queries,
connections, images in cells, other cell metadata, XML maps, revision history and dialog
sheets; in PowerPoint files after the first, zoom.

Examples:
  doc-merge deck-a.pptx deck-b.pptx -o all.pptx
  doc-merge q1.xlsx q2.xlsx q3.xlsx q4.xlsx -o year.xlsx
  doc-merge cover.docx chapters\\ appendix.docx -o book.docx
  doc-merge \"reports\\*.docx\" --styles first
  doc-merge a.pptx b.pptx --json
";

/// Whether a merge is under way, which a Ctrl+C asks to stop instead of ending the process.
static MERGING: AtomicBool = AtomicBool::new(false);

/// A Ctrl+C during a merge sets [`STOP`], so the temp file goes; any other time it just ends
/// the process with 130, the shells' code for it.
fn on_interrupt() -> bool {
    let merging = MERGING.load(Ordering::SeqCst);
    STOP.store(merging, Ordering::SeqCst);
    merging
}

#[cfg(unix)]
fn catch_interrupts() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
        fn _exit(status: i32) -> !;
    }
    extern "C" fn handler(_: i32) {
        if !on_interrupt() {
            // SAFETY: `_exit` is safe in a signal handler, unlike `process::exit`.
            unsafe { _exit(130) }
        }
    }
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    for signum in [SIGINT, SIGTERM] {
        // SAFETY: the handler only touches atomics and calls `_exit`.
        unsafe { signal(signum, handler) };
    }
}

#[cfg(windows)]
fn catch_interrupts() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::core::BOOL;
    unsafe extern "system" fn handler(_: u32) -> BOOL {
        if !on_interrupt() {
            std::process::exit(130);
        }
        1
    }
    // SAFETY: the handler runs on a thread of its own and only touches atomics.
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    // Only an exact first argument is a command: `./merge` is a file.
    match args.get(1).and_then(|a| a.to_str()) {
        Some("ui") => return command::<Ui>(&args[1..], "ui"),
        Some("mcp") => return command::<Mcp>(&args[1..], "mcp"),
        Some("merge") => drop(args.remove(1)),
        _ => {}
    }
    let json = args
        .iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--json");
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => return fail(&usage(&e), json),
    };
    if cli.help || cli.version {
        let version = format!("doc-merge {}\n", env!("CARGO_PKG_VERSION"));
        let text = if cli.help { HELP } else { &version };
        let _ = io::stdout().write_all(text.as_bytes());
        return ExitCode::SUCCESS;
    }
    catch_interrupts();
    let mut code = 0;
    match run(&cli, &mut io::stdout().lock(), &mut io::stderr(), &mut code) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => {
            fail(&Error::write_failed("the output stream", &e), false)
        }
        // A reader that hung up ends the run quietly with the code earned so far.
        _ => ExitCode::from(code),
    }
}

/// `ui` or `mcp`, once its arguments are read.
fn command<T: Parser>(args: &[OsString], name: &str) -> ExitCode {
    match T::try_parse_from(args) {
        Ok(_) => fail(
            &Error::internal(&format!("{name} isn't in this build")),
            false,
        ),
        Err(e) if e.use_stderr() => fail(&usage(&e), false),
        Err(e) => {
            let _ = e.print();
            ExitCode::SUCCESS
        }
    }
}

/// Merges `cli.paths` and prints how it went; `code` is set before anything is printed.
fn run(cli: &Cli, out: &mut impl Write, err: &mut impl Write, code: &mut u8) -> io::Result<()> {
    let target = Target {
        output: cli.output.clone(),
        overwrite: cli.overwrite,
    };
    let options = Options {
        styles: match cli.styles.as_deref() {
            Some("first") => StyleMode::First,
            _ => StyleMode::Keep,
        },
    };
    let merged = paths::expand(&cli.paths).and_then(|files| {
        MERGING.store(true, Ordering::SeqCst);
        let merged = guarded(|| merge(&files, &target, &options, None));
        MERGING.store(false, Ordering::SeqCst);
        merged
    });
    *code = merged.as_ref().err().map_or(0, |e| e.exit);
    if cli.debug {
        debug(err, &merged)?;
    }
    match merged {
        Ok(report) if cli.json => print_json(out, &report),
        Ok(report) => writeln!(out, "✓ {}", report.output.path.display()),
        Err(e) if cli.json => print_json(out, &Failed { error: &e }),
        Err(e) => print_error(err, &e),
    }
}

/// `--debug`: how long each step took, or the cause behind the error.
fn debug(err: &mut impl Write, merged: &Result<Report, Error>) -> io::Result<()> {
    match merged {
        Ok(report) => {
            for (step, ms) in report.steps {
                writeln!(err, "debug: {step} {ms} ms")?;
            }
        }
        Err(e) => {
            if let Some(cause) = &e.cause {
                writeln!(err, "debug: {}: {cause}", e.code)?;
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct Failed<'a> {
    error: &'a Error,
}

fn print_json(out: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, value)?;
    writeln!(out)?;
    out.flush()
}

fn print_error(err: &mut impl Write, e: &Error) -> io::Result<()> {
    writeln!(err, "✗ {}", e.message)?;
    match e.hint {
        Some(hint) => writeln!(err, "  {hint}"),
        None => Ok(()),
    }
}

/// A clap error as our usage error: `Unknown option --x.` or clap's own sentence.
fn usage(e: &clap::Error) -> Error {
    let message = match (e.kind(), e.get(ContextKind::InvalidArg)) {
        (ErrorKind::UnknownArgument, Some(ContextValue::String(arg))) => {
            format!("Unknown option {arg}.")
        }
        _ => {
            let text = e.render().to_string();
            let line = text.lines().next().unwrap_or_default();
            let line = line.strip_prefix("error: ").unwrap_or(line);
            let mut chars = line.chars();
            let first = chars.next().map(|c| c.to_uppercase().to_string());
            format!("{}{}.", first.unwrap_or_default(), chars.as_str())
        }
    };
    Error::usage(message)
}

/// Reports an error that stops the run, as two lines on stderr or as JSON on stdout.
fn fail(e: &Error, json: bool) -> ExitCode {
    let _ = if json {
        print_json(&mut io::stdout(), &Failed { error: e })
    } else {
        print_error(&mut io::stderr(), e)
    };
    ExitCode::from(e.exit)
}
