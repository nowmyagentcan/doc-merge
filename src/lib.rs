//! The merge engine: it reads Office packages, plans the merged one and writes it. It prints
//! nothing and does no terminal or network I/O; the command line, the MCP server and the page
//! all call it.

pub mod docx;
pub mod error;
pub mod package;
pub mod paths;
pub mod pptx;
pub mod writer;
pub mod xlsx;
pub mod xml;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Serialize, Serializer};

use error::{Error, Warnings, mib};
use package::{Family, Kind, Package};
use paths::Target;

/// Set on Ctrl+C: a run under way ends at its next look at the clock, removing its temp file.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// Opens the inputs after `first`, the first of `paths`, in order, then checks that they are
/// all of one family.
pub fn open_inputs(
    first: Package,
    paths: &[PathBuf],
    run: &Rc<Run>,
) -> Result<Vec<Package>, Error> {
    let mut pkgs = vec![first];
    for (i, p) in paths.iter().enumerate().skip(1) {
        pkgs.push(Package::open(p, i + 1, run)?);
    }
    let mut counts = [0; 3];
    for p in &pkgs {
        counts[p.kind.family() as usize] += 1;
    }
    if counts.iter().filter(|&&n| n > 0).count() > 1 {
        return Err(Error::mixed_formats(counts));
    }
    Ok(pkgs)
}

/// The most bytes one run inflates, over all its inputs.
pub const MAX_INFLATED: u64 = 4 << 30;
/// The largest size of all the inputs of a run together.
pub const MAX_INPUTS: u64 = 2 << 30;

/// What one merge may still spend: bytes to inflate and time.
#[derive(Debug)]
pub struct Run {
    inflate_left: Cell<u64>,
    deadline: Option<Instant>,
}

impl Run {
    pub fn new(deadline: Option<Instant>) -> Rc<Self> {
        Self::with_limit(deadline, MAX_INFLATED)
    }

    pub fn with_limit(deadline: Option<Instant>, inflate: u64) -> Rc<Self> {
        Rc::new(Self {
            inflate_left: Cell::new(inflate),
            deadline,
        })
    }

    /// Takes `n` inflated bytes from the run's allowance.
    pub fn inflate(&self, n: u64) -> Result<(), Error> {
        let left = self.inflate_left.get();
        if n > left {
            return Err(Error::limit_exceeded(
                "The data unpacked from the files",
                "more than 4,096 MiB",
                "4,096 MiB",
            ));
        }
        self.inflate_left.set(left - n);
        Ok(())
    }

    pub fn on_time(&self) -> Result<(), Error> {
        if STOP.load(Ordering::Relaxed) {
            return Err(Error::stopped());
        }
        match self.deadline {
            Some(d) if Instant::now() >= d => Err(Error::timeout()),
            _ => Ok(()),
        }
    }
}

/// Which input something came from, for its errors.
#[derive(Debug, Clone)]
pub struct Origin {
    /// The file's name as given.
    pub name: Rc<str>,
    /// 1-based.
    pub input: usize,
    pub run: Rc<Run>,
}

impl Origin {
    pub fn bad(&self, reason: &str) -> Error {
        Error::bad_package(&self.name, reason).at(self.input)
    }

    #[cfg(test)]
    pub(crate) fn test(name: &str) -> Self {
        Self {
            name: name.into(),
            input: 1,
            run: Run::new(None),
        }
    }
}

/// A path as text in JSON, also when it isn't valid Unicode.
fn lossy<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&path.to_string_lossy())
}

/// The merged file.
#[derive(Debug, Serialize)]
pub struct Output {
    #[serde(serialize_with = "lossy")]
    pub path: PathBuf,
    pub kind: Kind,
    pub bytes: u64,
}

/// One input and where its content landed.
#[derive(Debug, Serialize)]
pub struct Input {
    #[serde(serialize_with = "lossy")]
    pub path: PathBuf,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<usize>,
    /// 1-based, `[first, last]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_sections: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slides: Option<usize>,
    /// 1-based, `[first, last]`; `[0, 0]` for a deck without slides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_slides: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheets: Option<Vec<SheetName>>,
}

/// A sheet of an input and what it is called in the output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SheetName {
    pub name: String,
    pub output_name: String,
}

/// A name the merge changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rename {
    pub input: usize,
    pub kind: &'static str,
    pub from: String,
    pub to: String,
}

/// At most this many renames are listed; the rest are counted.
const MAX_RENAMES: usize = 1_000;

/// What a merge did, in the shape `--json` prints.
#[derive(Debug, Serialize)]
pub struct Report {
    pub output: Output,
    pub inputs: Vec<Input>,
    pub renames: Vec<Rename>,
    pub renames_omitted: usize,
    pub warnings: Warnings,
    pub elapsed_ms: u64,
    /// How long each step took, in milliseconds, for `--debug`.
    #[serde(skip)]
    pub steps: [(&'static str, u64); 3],
}

/// How clashing Word styles are settled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StyleMode {
    /// Each document keeps its look; clashing styles are copied under new names.
    #[default]
    Keep,
    /// The first document's definitions win.
    First,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub styles: StyleMode,
}

/// Merges `paths` (at least one; the first is the base) into a new file where `target` says.
/// The first file settles what kind the output is, so a wrong `-o` is found before a refusal
/// of a later file, as its exit code is higher.
pub fn merge(
    paths: &[PathBuf],
    target: &Target,
    options: &Options,
    deadline: Option<Instant>,
) -> Result<Report, Error> {
    let started = Instant::now();
    let total = paths.iter().flat_map(std::fs::metadata).map(|m| m.len());
    let total: u64 = total.sum();
    if total > MAX_INPUTS {
        let what = "The total size of the files";
        return Err(Error::limit_exceeded(what, &mib(total), &mib(MAX_INPUTS)));
    }
    let run = Run::new(deadline);
    let first = Package::open(&paths[0], 1, &run)?;
    let place = target.place(paths, first.kind.family())?;
    let mut inputs = open_inputs(first, paths, &run)?;
    let opened = Instant::now();
    let mut warnings = Warnings::default();
    let family = inputs[0].kind.family();
    let mut listed: Vec<Input> = inputs
        .iter()
        .map(|p| Input {
            path: std::path::absolute(&p.path).unwrap_or_else(|_| p.path.clone()),
            kind: p.kind,
            sections: None,
            output_sections: None,
            slides: None,
            output_slides: None,
            sheets: None,
        })
        .collect();
    let (plan, mut renames) = match family {
        Family::Word => {
            let m = docx::merge(&mut inputs, options.styles, &mut warnings)?;
            let mut next = 1;
            for (input, n) in listed.iter_mut().zip(m.sections) {
                input.sections = Some(n);
                input.output_sections = Some([next, next + n - 1]);
                next += n;
            }
            (m.plan, m.renames)
        }
        Family::Excel => {
            let m = xlsx::merge(&mut inputs, &mut warnings)?;
            for (input, sheets) in listed.iter_mut().zip(m.sheets) {
                input.sheets = Some(sheets);
            }
            (m.plan, m.renames)
        }
        Family::PowerPoint => {
            let m = pptx::merge(&mut inputs, &mut warnings)?;
            let mut next = 1;
            for (input, n) in listed.iter_mut().zip(m.slides) {
                input.slides = Some(n);
                input.output_slides = Some(if n == 0 { [0, 0] } else { [next, next + n - 1] });
                next += n;
            }
            (m.plan, m.renames)
        }
    };
    let planned = Instant::now();
    let (bytes, output) = writer::write(plan, &mut inputs, &place)?;
    let done = Instant::now();
    let ms = |from: Instant, to: Instant| (to - from).as_millis() as u64;
    renames.sort_by_key(|r| r.input);
    let renames_omitted = renames.len().saturating_sub(MAX_RENAMES);
    renames.truncate(MAX_RENAMES);
    Ok(Report {
        output: Output {
            path: std::path::absolute(&output).unwrap_or(output),
            kind: family.output(),
            bytes,
        },
        inputs: listed,
        renames,
        renames_omitted,
        warnings,
        elapsed_ms: ms(started, done),
        steps: [
            ("open", ms(started, opened)),
            ("plan", ms(opened, planned)),
            ("write", ms(planned, done)),
        ],
    })
}
