//! Errors.

/// What can go wrong reading captures or calibrating.
#[derive(Debug, thiserror::Error)]
pub enum TuneError {
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: String,
        /// The error.
        source: std::io::Error,
    },
    /// An input file is not in a format this crate reads, or is damaged.
    #[error("{0}")]
    Format(String),
    /// The session description is wrong.
    #[error("session: {0}")]
    Session(String),
    /// The captures do not allow a calibration step (e.g. no chart found).
    #[error("{0}")]
    Calibration(String),
    /// The tuning model rejected the result.
    #[error("tuning: {0}")]
    Tuning(#[from] styx_algo::AlgoError),
}

impl TuneError {
    /// An I/O error on `path`.
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

/// Result alias.
pub type Result<T> = std::result::Result<T, TuneError>;
