//! The merge engine: it reads Office packages, plans the merged one and writes it. It prints
//! nothing and does no terminal or network I/O; the command line, the MCP server and the page
//! all call it.

pub mod error;
pub mod package;
pub mod xml;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use error::Error;
use package::Package;

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
