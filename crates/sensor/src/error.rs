//! Errors.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::bus_error::BusError;

/// One problem found while validating a description, with the path of the offending value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Where in the description, e.g. `modes[1].crop` or `controls.exposure.register`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// All problems found in a description (validation does not stop at the first one).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Issues(pub Vec<Issue>);

impl fmt::Display for Issues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, issue) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "  {issue}")?;
        }
        Ok(())
    }
}

/// Errors from this crate.
#[derive(Debug, thiserror::Error)]
pub enum SensorError {
    /// The TOML did not parse or did not match the schema (unknown field, wrong type, ...).
    #[error("{source_name}: {message}")]
    Parse {
        /// File name or other label of the source.
        source_name: String,
        /// The parser's message, including line and column.
        message: String,
    },
    /// The description parsed but is inconsistent.
    #[error("{source_name}: invalid sensor description:\n{issues}")]
    Invalid {
        /// File name or other label of the source.
        source_name: String,
        /// Every problem found.
        issues: Issues,
    },
    /// Reading a description file failed.
    #[cfg(feature = "std")]
    #[error("reading {path}: {source}")]
    ReadFile {
        /// The path.
        path: String,
        /// The error.
        source: std::io::Error,
    },
    /// A register access failed.
    #[error("{op} register 0x{address:04x}: {source}")]
    Bus {
        /// `read` or `write`.
        op: &'static str,
        /// Register address.
        address: u16,
        /// The bus error.
        source: BusError,
    },
    /// A GPIO, clock or supply operation failed.
    #[error("{what} '{role}': {source}")]
    Pins {
        /// `gpio`, `clock` or `supply`.
        what: &'static str,
        /// The role name.
        role: String,
        /// The error.
        source: BusError,
    },
    /// The chip id read back did not match.
    #[error("chip id mismatch: expected one of {expected:x?}, read 0x{found:x}")]
    ChipId {
        /// Accepted values.
        expected: Vec<u32>,
        /// Value read.
        found: u32,
    },
    /// No mode with that name.
    #[error("unknown mode '{0}'")]
    UnknownMode(String),
    /// No format with that name, or the mode does not support it.
    #[error("mode '{mode}' has no format '{format}'")]
    UnknownFormat {
        /// Mode name.
        mode: String,
        /// Format name.
        format: String,
    },
    /// The description has no register for this control (e.g. a kernel-backed description).
    #[error("the description has no register for {0}")]
    NoRegister(&'static str),
    /// Setting V4L2 controls of a sensor a kernel driver owns failed.
    #[error("setting {controls}: {source}")]
    Controls {
        /// The controls and values, e.g. `EXPOSURE=642 ANALOGUE_GAIN=16`.
        controls: String,
        /// The error.
        source: BusError,
    },
    /// No test pattern with that name.
    #[error("unknown test pattern '{0}'")]
    UnknownTestPattern(String),
    /// The call is not valid in the driver's current state.
    #[error("invalid state: {0}")]
    State(&'static str),
}

/// Result alias.
pub type Result<T, E = SensorError> = core::result::Result<T, E>;
