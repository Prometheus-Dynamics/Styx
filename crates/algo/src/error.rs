//! Errors.

use thiserror::Error;

/// Errors from tuning, configuration and replay.
#[derive(Debug, Error)]
pub enum AlgoError {
    /// Tuning data is malformed or inconsistent. The string names what and where.
    #[error("tuning: {0}")]
    Tuning(String),
    /// A JSON document could not be parsed.
    #[error("json at byte {offset}: {message}")]
    Json {
        /// Byte offset of the problem.
        offset: usize,
        /// What went wrong.
        message: String,
    },
    /// TOML could not be parsed.
    #[error("toml: {0}")]
    Toml(String),
    /// The camera configuration cannot be used.
    #[error("config: {0}")]
    Config(String),
    /// A replay file is malformed.
    #[error("replay line {line}: {message}")]
    Replay {
        /// 1-based line number.
        line: usize,
        /// What went wrong.
        message: String,
    },
    /// I/O.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl AlgoError {
    pub(crate) fn tuning(msg: impl Into<String>) -> Self {
        Self::Tuning(msg.into())
    }
}

/// Result alias.
pub type Result<T, E = AlgoError> = std::result::Result<T, E>;
