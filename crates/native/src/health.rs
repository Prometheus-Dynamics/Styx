//! What the event thread and the frame stream learn about a running stream's health: the first
//! fault that ends it, and counters.

use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::control::lock;
use crate::error::NativeError;

/// Consecutive failed control writes at frame start after which the sensor counts as gone.
pub(crate) const MAX_CONTROL_FAILURES: u32 = 8;

/// A problem that ends a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// A device node went away (module unloaded, overlay removed, unbound).
    Disconnected(String),
    /// A kernel call failed with an errno.
    Kernel { what: String, errno: Option<i32> },
}

impl Fault {
    pub(crate) fn from_io(what: impl Into<String>, e: &io::Error) -> Self {
        let what = what.into();
        let errno = crate::error::io_errno(e);
        if matches!(errno, Some(libc::ENODEV) | Some(libc::ENXIO)) {
            Fault::Disconnected(format!("{what}: {e}"))
        } else {
            Fault::Kernel {
                what: format!("{what}: {e}"),
                errno,
            }
        }
    }

    pub(crate) fn to_error(&self) -> NativeError {
        match self {
            Fault::Disconnected(_) => NativeError::Disconnected,
            Fault::Kernel { what, errno } => {
                let source =
                    errno.map_or_else(|| io::Error::other("failed"), io::Error::from_raw_os_error);
                NativeError::kernel(what.clone(), source)
            }
        }
    }
}

/// Shared by the event thread, the frame stream and the camera.
#[derive(Debug, Default)]
pub(crate) struct Health {
    fault: Mutex<Option<Fault>>,
    /// Why the sensor failed to serve the last request it failed, if one did.
    serve_error: Mutex<Option<String>>,
    /// Stream requests acknowledged.
    pub(crate) acks: AtomicU64,
    /// Frame-start events seen.
    pub(crate) frame_syncs: AtomicU64,
    /// Control writes at frame start that failed.
    pub(crate) control_failures: AtomicU64,
    consecutive_control_failures: AtomicU32,
    /// Start requests served after the bridge gave up waiting (the sensor was put back in
    /// standby).
    pub(crate) late_acks: AtomicU64,
}

impl Health {
    /// Records the fault that ends the stream (the first one wins).
    pub(crate) fn fail(&self, fault: Fault) {
        let mut f = lock(&self.fault);
        if f.is_none() {
            *f = Some(fault);
        }
    }

    pub(crate) fn fault(&self) -> Option<Fault> {
        lock(&self.fault).clone()
    }

    pub(crate) fn serve_failed(&self, why: String) {
        *lock(&self.serve_error) = Some(why);
    }

    pub(crate) fn serve_error(&self) -> Option<String> {
        lock(&self.serve_error).clone()
    }

    /// A control write at frame start succeeded or failed; enough failures in a row end the
    /// stream (the sensor stopped answering).
    pub(crate) fn control_write(&self, result: std::result::Result<(), String>) {
        match result {
            Ok(()) => self
                .consecutive_control_failures
                .store(0, Ordering::Relaxed),
            Err(why) => {
                self.control_failures.fetch_add(1, Ordering::Relaxed);
                let n = self
                    .consecutive_control_failures
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                if n >= MAX_CONTROL_FAILURES {
                    self.fail(Fault::Kernel {
                        what: format!(
                            "the sensor stopped answering: {n} control writes failed in a row \
                             (last: {why})"
                        ),
                        errno: Some(libc::EREMOTEIO),
                    });
                }
            }
        }
    }
}
