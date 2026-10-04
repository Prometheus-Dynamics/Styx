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
