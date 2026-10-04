//! What the event thread and the frame stream learn about a running stream's health: the
//! runtime's [`Health`] (the first fault that ends it, and counters), with Linux's errno
//! classification on the way in and [`NativeError`]s on the way out.

use std::io;

use styx_runtime::styx_hal::ErrorKind;
pub(crate) use styx_runtime::{Fault, Health};

use crate::error::NativeError;

/// The fault of a failed kernel call: a device that went away (`ENODEV`, `ENXIO`), else the
/// call's error with its errno.
pub(crate) fn fault_from_io(what: impl Into<String>, e: &io::Error) -> Fault {
    let what = what.into();
    let errno = crate::error::io_errno(e);
    if matches!(errno, Some(libc::ENODEV) | Some(libc::ENXIO)) {
        Fault::disconnected(format!("{what}: {e}"))
    } else {
        Fault::new(ErrorKind::from_io(e), format!("{what}: {e}"), errno)
    }
}

/// The error a fault ends the stream with.
pub(crate) fn fault_error(f: &Fault) -> NativeError {
    if f.is_disconnect() {
        return NativeError::Disconnected;
    }
    let errno = f.code.or(match f.kind {
        // The sensor stopped answering on I²C.
        ErrorKind::Nack => Some(libc::EREMOTEIO),
        ErrorKind::Corrupt => Some(libc::EIO),
        _ => None,
    });
    let source = errno.map_or_else(|| io::Error::other("failed"), io::Error::from_raw_os_error);
    NativeError::kernel(f.what.clone(), source)
}

/// What kind of hardware failure an errno is: `ENODEV`/`ENXIO` mean the device went away.
pub(crate) fn errno_kind(errno: Option<i32>, e: &io::Error) -> ErrorKind {
    match errno {
        Some(libc::ENODEV) | Some(libc::ENXIO) => ErrorKind::Disconnected,
        Some(libc::EBUSY) => ErrorKind::Busy,
        Some(libc::ETIMEDOUT) => ErrorKind::Timeout,
        Some(libc::EREMOTEIO) => ErrorKind::Nack,
        Some(libc::ENOMEM) => ErrorKind::NoMemory,
        _ => ErrorKind::from_io(e),
    }
}

/// A kernel call's error as a [`HalError`](styx_runtime::styx_hal::HalError): the I/O error,
/// with `ENODEV`/`ENXIO` as [`ErrorKind::Disconnected`].
#[derive(Debug)]
pub(crate) struct IoError(pub(crate) io::Error);

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl styx_runtime::styx_hal::HalError for IoError {
    fn kind(&self) -> ErrorKind {
        errno_kind(crate::error::io_errno(&self.0), &self.0)
    }
    fn code(&self) -> Option<i32> {
        crate::error::io_errno(&self.0)
    }
}

impl styx_runtime::styx_hal::HalError for NativeError {
    fn kind(&self) -> ErrorKind {
        match self {
            e if e.is_disconnect() => ErrorKind::Disconnected,
            NativeError::Kernel { source, .. } => errno_kind(self.errno(), source),
            NativeError::Busy(_) => ErrorKind::Busy,
            NativeError::Timeout => ErrorKind::Timeout,
            NativeError::InvalidConfig(_) | NativeError::NoDescription(..) => {
                ErrorKind::InvalidConfig
            }
            NativeError::Topology(_) => ErrorKind::NotFound,
            NativeError::Sensor(_) => ErrorKind::Io,
            _ => ErrorKind::Other,
        }
    }
    fn code(&self) -> Option<i32> {
        self.errno()
    }
}
