//! What a running stream learns about its health: the first fault that ends it, and counters.
//! Shared by whatever serves frame starts (the Linux event thread, a sensor task), the frame
//! stream and the camera.

use alloc::format;
use alloc::string::String;
use styx_core::sync::{AtomicU32, Ordering};

use styx_hal::ErrorKind;

use crate::sync::{Counter, Lock, lock, new_lock};

/// Consecutive failed control writes at frame start after which the sensor counts as gone.
pub const MAX_CONTROL_FAILURES: u32 = 8;

/// A problem that ends a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    /// What kind: [`ErrorKind::Disconnected`] when a device went away, [`ErrorKind::Nack`] when
    /// the sensor stopped answering, [`ErrorKind::Corrupt`] for too many corrupted frames in a
    /// row, else what the failing call said.
    pub kind: ErrorKind,
    /// What failed.
    pub what: String,
    /// A platform code (an errno on Linux), if known.
    pub code: Option<i32>,
}

impl Fault {
    /// A fault of `kind`.
    pub fn new(kind: ErrorKind, what: impl Into<String>, code: Option<i32>) -> Self {
        Self {
            kind,
            what: what.into(),
            code,
        }
    }

    /// A device went away.
    pub fn disconnected(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::Disconnected, what, None)
    }

    /// Whether a device went away.
    pub fn is_disconnect(&self) -> bool {
        self.kind == ErrorKind::Disconnected
    }
}

/// The health of a running stream.
#[derive(Debug)]
pub struct Health {
    fault: Lock<Option<Fault>>,
    /// Why the sensor failed to serve the last start or stop it failed, if one did.
    serve_error: Lock<Option<String>>,
    /// Start and stop requests acknowledged (a receiver that asks for them).
    pub acks: Counter,
    /// Frame-start events seen.
    pub frame_syncs: Counter,
    /// Control writes at frame start that failed.
    pub control_failures: Counter,
    consecutive_control_failures: AtomicU32,
    /// Start requests served after the receiver gave up waiting (the sensor was put back in
    /// standby).
    pub late_acks: Counter,
    /// Non-fatal receiver problems ([`SyncEvent::Glitch`](styx_hal::SyncEvent::Glitch)).
    pub glitches: Counter,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            fault: new_lock(None),
            serve_error: new_lock(None),
            acks: Counter::new(),
            frame_syncs: Counter::new(),
            control_failures: Counter::new(),
            consecutive_control_failures: AtomicU32::new(0),
            late_acks: Counter::new(),
            glitches: Counter::new(),
        }
    }
}

impl Health {
    /// Records the fault that ends the stream (the first one wins).
    pub fn fail(&self, fault: Fault) {
        let mut f = lock(&self.fault);
        if f.is_none() {
            *f = Some(fault);
        }
    }

    /// The fault that ended the stream, if one did.
    pub fn fault(&self) -> Option<Fault> {
        lock(&self.fault).clone()
    }

    /// Whether a fault ended the stream.
    pub fn has_fault(&self) -> bool {
        lock(&self.fault).is_some()
    }

    /// The sensor failed to serve a start or stop: why.
    pub fn serve_failed(&self, why: String) {
        *lock(&self.serve_error) = Some(why);
    }

    /// Why the sensor failed to serve the last start or stop it failed, if one did.
    pub fn serve_error(&self) -> Option<String> {
        lock(&self.serve_error).clone()
    }

    /// A control write at frame start succeeded or failed; [`MAX_CONTROL_FAILURES`] failures in
    /// a row end the stream (the sensor stopped answering: [`ErrorKind::Nack`]).
    pub fn control_write(&self, result: Result<(), String>) {
        match result {
            Ok(()) => self
                .consecutive_control_failures
                .store(0, Ordering::Relaxed),
            Err(why) => {
                self.control_failures.incr();
                let n = self
                    .consecutive_control_failures
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                if n >= MAX_CONTROL_FAILURES {
                    self.fail(Fault::new(
                        ErrorKind::Nack,
                        format!(
                            "the sensor stopped answering: {n} control writes failed in a row \
                             (last: {why})"
                        ),
                        None,
                    ));
                }
            }
        }
    }
}
