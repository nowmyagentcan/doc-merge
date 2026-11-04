//! The merge engine: it reads Office packages, plans the merged one and writes it. It prints
//! nothing and does no terminal or network I/O; the command line, the MCP server and the page
//! all call it.

pub mod docx;
pub mod error;
pub mod package;
pub mod writer;
pub mod xlsx;
pub mod xml;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use serde::Serialize;

use error::{Error, Warnings};
use package::{Family, Kind, Package};

/// Opens every input in order, then checks that they are all of one family.
pub fn open_inputs(paths: &[PathBuf], run: &Rc<Run>) -> Result<Vec<Package>, Error> {
    let pkgs = paths
        .iter()
        .enumerate()
        .map(|(i, p)| Package::open(p, i + 1, run))
        .collect::<Result<Vec<_>, _>>()?;
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

/// The merged file.
#[derive(Debug, Serialize)]
pub struct Output {
    pub path: PathBuf,
    pub kind: Kind,
    pub bytes: u64,
}

/// One input and where its content landed.
#[derive(Debug, Serialize)]
pub struct Input {
    pub path: PathBuf,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<usize>,
    /// 1-based, `[first, last]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_sections: Option<[usize; 2]>,
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

/// Merges `paths` (the first is the base) into a new file at `output`.
pub fn merge(
    paths: &[PathBuf],
    output: &Path,
    options: &Options,
    deadline: Option<Instant>,
) -> Result<Report, Error> {
    let started = Instant::now();
    let run = Run::new(deadline);
    let mut inputs = open_inputs(paths, &run)?;
    let mut warnings = Warnings::default();
    let family = inputs[0].kind.family();
    let mut listed: Vec<Input> = inputs
        .iter()
        .map(|p| Input {
            path: std::path::absolute(&p.path).unwrap_or_else(|_| p.path.clone()),
            kind: p.kind,
            sections: None,
            output_sections: None,
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
        _ => return Err(Error::internal("PowerPoint decks can't be merged so far")),
    };
    let bytes = writer::write(plan, &mut inputs, output)?;
    let renames_omitted = renames.len().saturating_sub(MAX_RENAMES);
    renames.truncate(MAX_RENAMES);
    Ok(Report {
        output: Output {
            path: std::path::absolute(output).unwrap_or_else(|_| output.into()),
            kind: family.output(),
            bytes,
        },
        inputs: listed,
        renames,
        renames_omitted,
        warnings,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}
