//! Whether the CPU can read a frame, and how fast.

use core::fmt;

use crate::sync::{AtomicBool, Mutex, Ordering};

/// Whether the CPU can read a frame's planes, and at what cost. Where the memory lives
/// ([`FrameResidency`](crate::buffer::FrameResidency)) does not decide it: a dma-buf mapped from a cached heap reads as fast as
/// heap memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CpuAccess {
    /// Not readable by the CPU (a GPU texture, a dma-buf that is not mapped or cannot be).
    None,
    /// Readable, but uncached or write-combined: every pass goes to memory, many times slower
    /// than cached reads. Copy once if the frame is read more than once.
    Uncached,
    /// Readable at memory speed: cached (dma-bufs are synced for the CPU before reading).
    Cached,
}

impl CpuAccess {
    /// Whether the CPU can read the planes at all.
    pub fn readable(self) -> bool {
        self != CpuAccess::None
    }
}

impl fmt::Display for CpuAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Uncached => write!(f, "uncached"),
            Self::Cached => write!(f, "cached"),
        }
    }
}

/// When a backing over device-written memory brackets the CPU's reads with cache maintenance (a
/// dma-buf's `DMA_BUF_IOCTL_SYNC` START / END, a D-cache invalidate): the one place that counts
/// them, for backings read two ways.
///
/// - Held reads ([`ExternalBacking::plane_data`](crate::buffer::ExternalBacking::plane_data),
///   [`FrameLease::planes`](crate::buffer::FrameLease::planes)): the first opens CPU access
///   ([`CpuReadWindow::hold`]), which stays open until the backing drops
///   ([`CpuReadWindow::close`]). No end call follows such reads.
/// - Bracketed reads ([`ExternalBacking::begin_cpu_read`](crate::buffer::ExternalBacking::begin_cpu_read)
///   / [`end_cpu_read`](crate::buffer::ExternalBacking::end_cpu_read), Daedalus's
///   `daedalus:frame` access): counted and possibly overlapping (several consumers read one
///   frame); the first to begin opens CPU access, the last to end closes it, unless a held read
///   keeps it open.
///
/// The hooks run under a lock, so a second reader waits until the first one's START is done.
/// A backing that is never read never syncs.
pub struct CpuReadWindow {
    /// A held read opened the window (fast path, set once).
    held: AtomicBool,
    /// Open bracketed reads, and whether the window is open.
    state: Mutex<(u32, bool)>,
}

impl Default for CpuReadWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuReadWindow {
    /// Closed, nothing read.
    pub const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            state: Mutex::new((0, false)),
        }
    }

    /// A held read: runs `begin` (START) unless the window is open, and keeps it open until
    /// [`CpuReadWindow::close`].
    pub fn hold(&self, begin: impl FnOnce()) {
        if self.held.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock();
        if !state.1 {
            begin();
            state.1 = true;
        }
        self.held.store(true, Ordering::Release);
    }

    /// Begins a bracketed read: runs `begin` (START) when the window is closed. Pair it with one
    /// [`CpuReadWindow::end`].
    pub fn begin(&self, begin: impl FnOnce()) {
        let mut state = self.state.lock();
        if !state.1 {
            begin();
            state.1 = true;
        }
        state.0 = state.0.saturating_add(1);
    }

    /// Ends a bracketed read: runs `end` (END) when it was the last open one and no held read
    /// keeps the window open. Nothing without an open bracketed read.
    pub fn end(&self, end: impl FnOnce()) {
        let mut state = self.state.lock();
        if state.0 == 0 {
            return;
        }
        state.0 -= 1;
        if state.0 == 0 && state.1 && !self.held.load(Ordering::Acquire) {
            end();
            state.1 = false;
        }
    }

    /// Whether CPU access is open.
    pub fn is_open(&self) -> bool {
        self.state.lock().1
    }

    /// The backing is done (it drops): runs `end` (END) when the window is open.
    pub fn close(&self, end: impl FnOnce()) {
        let mut state = self.state.lock();
        if state.1 {
            end();
            state.1 = false;
        }
        state.0 = 0;
        self.held.store(false, Ordering::Release);
    }
}

impl fmt::Debug for CpuReadWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
        f.debug_struct("CpuReadWindow")
            .field("held", &self.held.load(Ordering::Relaxed))
            .field("bracketed", &state.0)
            .field("open", &state.1)
            .finish()
    }
}
