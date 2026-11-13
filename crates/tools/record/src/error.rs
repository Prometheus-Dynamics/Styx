//! Why a recording could not be written or read.

use core::fmt;
use std::io;

/// A refusal or a failure. Input that breaks the format is refused with [`RecordError::Corrupt`]
/// or [`RecordError::TooLarge`]; nothing is allocated for it and nothing panics.
#[derive(Debug)]
#[non_exhaustive]
pub enum RecordError {
    /// The file could not be read or written.
    Io(io::Error),
    /// The file does not start with the `styxrec` magic.
    NotARecording,
    /// A `styxrec` version this build does not read.
    UnsupportedVersion(u16),
    /// The input breaks the format; the message says what.
    Corrupt(&'static str),
    /// A length or value above its limit.
    TooLarge(&'static str),
    /// Input this build does not read (for example a compressed MCAP chunk).
    Unsupported(&'static str),
    /// A frame cannot be recorded or replayed (its layout, data or buffer does not fit).
    Frame(String),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "recording i/o: {e}"),
            Self::NotARecording => write!(f, "not a styxrec recording"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported styxrec version {v}"),
            Self::Corrupt(what) => write!(f, "corrupt recording: {what}"),
            Self::TooLarge(what) => write!(f, "recording limit exceeded: {what}"),
            Self::Unsupported(what) => write!(f, "unsupported recording: {what}"),
            Self::Frame(what) => write!(f, "frame: {what}"),
        }
    }
}

impl std::error::Error for RecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for RecordError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
