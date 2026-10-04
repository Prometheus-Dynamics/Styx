//! Whether the CPU can read a frame, and how fast.

use core::fmt;

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
