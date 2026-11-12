//! The Styx frame.
//!
//! Buffers are given once: a [`FramePool`] is built over a fixed set of buffers, each in one kind
//! of memory ([`MemoryKind`]: host, memfd, dma-buf or GPU), described by a [`BufferMemory`]. After
//! that, every frame is an entry in one of those buffers ([`FrameEntry`]: a `u64` identity, the
//! format, the plane layout, and a `CLOCK_MONOTONIC` timestamp in nanoseconds), handed out as a
//! [`FrameLease`].
//!
//! - Steady state allocates nothing. Taking a buffer, sharing a frame and releasing it touch only
//!   atomics in memory set up when the pool was built: no `Arc` per frame, no clone of the layout
//!   or metadata per share.
//! - A pool miss returns [`AcquireError::Exhausted`]. It never allocates and never zero-fills; the
//!   producer drops the frame and the miss is counted.
//! - Shares are read-only. A frame's planes, entry and metadata are written only while one lease
//!   holds it ([`FrameLease::is_sole_owner`]).
//! - CPU access is bracketed ([`BufferMemory::begin_cpu`] / [`BufferMemory::end_cpu`]): a dma-buf
//!   gets `DMA_BUF_IOCTL_SYNC` START when its first reader opens and END when its last closes, and
//!   the same around a write.
//! - Every buffer carries the slack a GPU reads past its end ([`BufferSize`]), asked for once when
//!   the buffers are made, so importing one into a GPU is never refused for length.
//! - Every copy goes through a [`CopyCounter`] at the place it happens, so a test can assert none.
//!
//! `no_std` + `alloc`; the `std` feature adds error traits and file descriptors, `linux` adds
//! memfd and dma-buf memory.

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod copies;
mod entry;
#[cfg(all(feature = "linux", target_os = "linux"))]
mod fd;
mod layout;
mod lease;
mod memory;
mod pool;

pub use copies::{COPY_INTO, CopyCounter, copy_counters};
pub use entry::FrameEntry;
#[cfg(all(feature = "linux", target_os = "linux"))]
pub use fd::FdMemory;
pub use layout::{Layouts, PlaneLayout, packed_layouts};
pub use lease::{CopyError, CpuRead, FrameLease, Plane, PlaneMut, PlanesMut, SetError};
pub use memory::{BufferMemory, CpuAccess, HostMemory, MemoryKind};
pub use pool::{AcquireError, BufferSize, FramePool, PoolError, PoolStats};
pub use styx_format::{MAX_PLANES, MediaFormat};
