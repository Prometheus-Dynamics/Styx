//! The memory a simulated board's streams are made of, for tests that run a session on each.
//!
//! A board stream is shared memory (one memfd) unless its board file says `memory = "dma-buf"`:
//! then the daemon makes one dma-buf per buffer from the stream's heap ([`DEFAULT_HEAP`] here),
//! and a stream that cannot have them is refused, never quietly made of shared memory. A test
//! of the dma-buf path asks [`dma_buf_case`] first: on a host with no heap it may use, the case
//! fails or is recorded as skipped by `styx_frame::dma_case`'s rule.

use std::fs::File;
use std::os::fd::AsFd;

pub use lemnos_host::board::DEFAULT_HEAP;

/// What a simulated board's streams are made of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Memory {
    /// One memfd per stream, the board file's default.
    #[default]
    Shared,
    /// One dma-buf per buffer, from [`DEFAULT_HEAP`].
    DmaBuf,
}

impl Memory {
    /// The line a board file's `[devices.streams.<name>]` section takes for this memory, with its
    /// newline: nothing for shared memory.
    #[must_use]
    pub const fn line(self) -> &'static str {
        match self {
            Self::Shared => "",
            Self::DmaBuf => "memory = \"dma-buf\"\n",
        }
    }
}

/// Whether this host gives the dma-bufs a `memory = "dma-buf"` stream is made of: one page is
/// allocated from [`DEFAULT_HEAP`] and given back. The error says why not, for the test to print.
///
/// # Errors
///
/// The heap is not there, this user may not open it, or it gives no buffer.
pub fn dma_heap_here() -> Result<(), String> {
    let path = format!("/dev/dma_heap/{DEFAULT_HEAP}");
    let heap = File::open(&path).map_err(|e| format!("{path} is not openable here: {e}"))?;
    lemnos_linux_sys::dma_heap::allocate(heap.as_fd(), 4096)
        .map(drop)
        .map_err(|e| format!("{path} gives no buffer: {e}"))
}

/// Whether dma-buf case `case` can run here. When it cannot, `styx_frame::dma_case` decides:
/// the case fails under `STYX_REQUIRE_DMA_HEAP=1`, or is recorded as skipped and this is `false`.
#[must_use]
pub fn dma_buf_case(case: &str) -> bool {
    match dma_heap_here() {
        Ok(()) => true,
        Err(why) => {
            styx_frame::dma_case::unavailable(case, &why);
            false
        }
    }
}
