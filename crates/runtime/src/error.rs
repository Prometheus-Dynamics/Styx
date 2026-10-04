//! The runtime's error: what failed and what kind of failure it was, so a platform maps it to
//! its own error type without losing the decisions it needs (a disconnect ends the stream and
//! lets the supervisor reconnect, `Busy` means exclusive open, a sensor that does not answer
//! keeps its bus error).

use alloc::string::String;

use styx_hal::{ErrorKind, HalError};
use styx_sensor::{BusError, SensorError};

use crate::health::Fault;

/// What can go wrong in the runtime.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The sensor driver (description, bus, state) failed.
    #[error("sensor: {0}")]
    Sensor(#[from] SensorError),
    /// A register bus or pin operation outside the driver failed (`during` says which step).
    #[error("{during}: {source}")]
    Bus {
        /// The step.
        during: &'static str,
        /// The bus error (with `std`, it keeps the I/O error and its errno).
        source: BusError,
    },
    /// A hardware trait implementation (receiver, memory, lens) failed.
    #[error("{during}: {message}")]
    Hal {
        /// The step.
        during: &'static str,
        /// What kind of failure.
        kind: ErrorKind,
        /// A platform code (an errno on Linux).
        code: Option<i32>,
        /// The implementation's message.
        message: String,
    },
    /// The request does not fit the sensor or the receiver.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// The camera is in the wrong state for the call.
    #[error("{0}")]
    State(&'static str),
    /// The camera is in use.
    #[error("busy: {0}")]
    Busy(String),
    /// The device went away while in use.
    #[error("device disconnected")]
    Disconnected,
    /// Waiting timed out.
    #[error("timed out")]
    Timeout,
    /// A fault ended the stream (see [`Fault`]).
    #[error("{}", .0.what)]
    Fault(Fault),
}

/// Result of the runtime.
pub type Result<T> = core::result::Result<T, Error>;

impl Error {
    /// The error of a hardware trait implementation during `during`.
    pub fn hal<E: HalError>(during: &'static str, e: &E) -> Self {
        Error::Hal {
            during,
            kind: e.kind(),
            code: e.code(),
            message: alloc::string::ToString::to_string(e),
        }
    }

    /// What kind of failure this is.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Sensor(SensorError::Bus { source, .. }) | Error::Bus { source, .. } => {
                HalError::kind(source)
            }
            Error::Sensor(_) | Error::InvalidConfig(_) => ErrorKind::InvalidConfig,
            Error::Hal { kind, .. } => *kind,
            Error::State(_) => ErrorKind::Other,
            Error::Busy(_) => ErrorKind::Busy,
            Error::Disconnected => ErrorKind::Disconnected,
            Error::Timeout => ErrorKind::Timeout,
            Error::Fault(f) => f.kind,
        }
    }

    /// Whether the device went away.
    pub fn is_disconnect(&self) -> bool {
        self.kind() == ErrorKind::Disconnected
    }
}
