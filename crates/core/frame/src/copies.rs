//! Copy counters: one per place a frame's bytes are copied.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Counts the copies made at one site and the bytes they moved. Declared as a `static` where the
/// copy happens, so a test can read it and assert none were made.
pub struct CopyCounter {
    site: &'static str,
    copies: AtomicUsize,
    bytes: AtomicUsize,
}

impl CopyCounter {
    pub const fn new(site: &'static str) -> Self {
        Self {
            site,
            copies: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
        }
    }

    /// One copy of `bytes` bytes.
    pub fn record(&self, bytes: usize) {
        self.copies.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn site(&self) -> &'static str {
        self.site
    }

    pub fn copies(&self) -> usize {
        self.copies.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for CopyCounter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopyCounter")
            .field("site", &self.site)
            .field("copies", &self.copies())
            .field("bytes", &self.bytes())
            .finish()
    }
}

/// [`crate::FrameLease::copy_into`]: the one place this crate copies a frame's bytes.
pub static COPY_INTO: CopyCounter = CopyCounter::new("styx-frame: FrameLease::copy_into");

/// Every copy site in this crate.
pub fn copy_counters() -> &'static [&'static CopyCounter] {
    static ALL: [&CopyCounter; 1] = [&COPY_INTO];
    &ALL
}
