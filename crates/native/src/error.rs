//! Errors of the native runtime.

use std::io;

use styx_graph::ProviderError;

/// What can go wrong driving a native camera.
#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    /// A kernel interface failed (`what` says which step).
    #[error("{what}: {source}")]
    Kernel {
        /// The step that failed.
        what: String,
        /// The kernel error.
        #[source]
        source: io::Error,
    },
    /// The sensor driver (description, bus, state) failed.
    #[error("sensor: {0}")]
    Sensor(#[from] styx_sensor::SensorError),
    /// No sensor description matches the bridge's sensor.
    #[error("no sensor description for \"{0}\" (searched {1})")]
    NoDescription(String, String),
    /// The media graph does not have the shape this runtime drives.
    #[error("media graph: {0}")]
    Topology(String),
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
    /// Waiting for a frame timed out.
    #[error("timed out")]
    Timeout,
}

/// The errno of an I/O error: its own, or that of a `styx_kernel::Error` it wraps (the
/// conversion from a kernel error keeps only the error kind on the outside).
pub(crate) fn io_errno(e: &io::Error) -> Option<i32> {
    e.raw_os_error().or_else(|| {
        e.get_ref()
            .and_then(|inner| inner.downcast_ref::<styx_kernel::Error>())
            .and_then(styx_kernel::Error::errno)
    })
}

/// Result of the native runtime.
pub type Result<T> = std::result::Result<T, NativeError>;

impl NativeError {
    /// A kernel error with the step it came from.
    pub fn kernel(what: impl Into<String>, source: impl Into<io::Error>) -> Self {
        NativeError::Kernel {
            what: what.into(),
            source: source.into(),
        }
    }

    /// The errno behind a kernel error, if any (also through a wrapped `styx_kernel::Error`).
    pub fn errno(&self) -> Option<i32> {
        match self {
            NativeError::Kernel { source, .. } => io_errno(source),
            _ => None,
        }
    }

    /// True when the error means the device node disappeared (`ENODEV`, `ENXIO`).
    pub fn is_disconnect(&self) -> bool {
        matches!(self, NativeError::Disconnected)
            || matches!(self.errno(), Some(libc::ENODEV) | Some(libc::ENXIO))
    }
}

/// Adds the step to a kernel error.
pub(crate) trait KernelContext<T> {
    fn step(self, what: &str) -> Result<T>;
}

impl<T, E: Into<io::Error>> KernelContext<T> for std::result::Result<T, E> {
    fn step(self, what: &str) -> Result<T> {
        self.map_err(|e| NativeError::kernel(what, e))
    }
}

impl From<NativeError> for ProviderError {
    fn from(e: NativeError) -> Self {
        match e {
            e if e.is_disconnect() => ProviderError::Disconnected,
            NativeError::Busy(what) => ProviderError::Busy(what),
            NativeError::InvalidConfig(what) => ProviderError::InvalidConfig(what),
            NativeError::Kernel { what, source } => {
                ProviderError::Io(io::Error::new(source.kind(), format!("{what}: {source}")))
            }
            other => ProviderError::Io(io::Error::other(other.to_string())),
        }
    }
}
