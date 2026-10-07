//! Ctrl-C (SIGINT) and SIGTERM stop the recording cleanly: the handler only sets a flag the
//! recording loop checks; the files are then flushed and finalised as at a normal end.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Install the handlers; returns the flag they set.
pub fn install() -> &'static AtomicBool {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: a zeroed `sigaction` is valid; the handler only stores to an atomic, which is
        // async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            // Restart interrupted reads (stdin for --on-key); the loop polls the flag.
            action.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
    &STOP
}
