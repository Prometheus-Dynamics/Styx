//! A hold on one frame.

use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::Ordering;

use styx_format::{LayoutError, MediaFormat, plane_geometry};

use crate::copies::COPY_INTO;
use crate::entry::FrameEntry;
use crate::layout::{Layouts, PlaneLayout};
use crate::memory::{BufferMemory, CpuAccess, MemoryKind};
use crate::pool::{AcquireError, Shared, Slot};

/// A hold on one frame in a [`crate::FramePool`] buffer.
///
/// The buffer goes back to the pool when the last lease on it drops. [`FrameLease::share`] (or
/// `clone`) gives another lease on the same frame: one atomic increment, nothing allocated,
/// nothing copied. While more than one lease is out the frame is read-only; the lease that is the
/// only one left may write it again.
pub struct FrameLease<M = ()> {
    shared: Arc<Shared<M>>,
    index: usize,
}

impl<M> FrameLease<M> {
    pub(crate) fn new(shared: Arc<Shared<M>>, index: usize) -> Self {
        Self { shared, index }
    }

    fn slot(&self) -> &Slot<M> {
        &self.shared.slots[self.index]
    }

    /// The frame's entry: identity, timestamp, format and plane layout.
    pub fn entry(&self) -> &FrameEntry {
        // SAFETY: the entry is written only by `acquire` before this lease existed or through
        // `&mut self` while this lease is the only one; this `&self` borrow excludes the latter.
        unsafe { &*self.slot().entry.get() }
    }

    /// The frame's identity ([`FrameEntry::id`]).
    pub fn id(&self) -> u64 {
        self.entry().id
    }

    /// `CLOCK_MONOTONIC` nanoseconds ([`FrameEntry::timestamp_ns`]).
    pub fn timestamp_ns(&self) -> u64 {
        self.entry().timestamp_ns
    }

    pub fn format(&self) -> &MediaFormat {
        &self.entry().format
    }

    pub fn layouts(&self) -> &Layouts {
        &self.entry().layouts
    }

    /// The per-frame metadata kept beside the entry.
    pub fn meta(&self) -> &M {
        // SAFETY: as for `entry`: written only through `&mut self` while this lease is the only
        // one.
        unsafe { &*self.slot().meta.get() }
    }

    /// The buffer's index in its pool; the same for every frame written into it.
    pub fn buffer_index(&self) -> usize {
        self.index
    }

    pub fn memory(&self) -> &dyn BufferMemory {
        self.slot().memory()
    }

    pub fn memory_kind(&self) -> MemoryKind {
        self.slot().memory().kind()
    }

    /// Leases on this frame now, this one included.
    pub fn holders(&self) -> u32 {
        self.slot().refs.load(Ordering::Acquire)
    }

    /// Whether this is the only lease: then the frame may be written.
    pub fn is_sole_owner(&self) -> bool {
        self.holders() == 1
    }

    /// Another lease on the same frame. Ends an open write window first, so the share sees the
    /// written bytes.
    pub fn share(&self) -> Self {
        self.finish_write();
        self.slot().refs.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
            index: self.index,
        }
    }

    /// Replaces the entry, while this is the only lease. The planes must fit the buffer's payload.
    pub fn set_entry(&mut self, entry: FrameEntry) -> Result<(), SetError> {
        if !self.is_sole_owner() {
            return Err(SetError::Shared);
        }
        let len = self.shared.size.payload;
        match entry.layouts.span() {
            Some(needed) if needed <= len => {}
            needed => {
                return Err(SetError::TooSmall(AcquireError::TooSmall {
                    needed: needed.unwrap_or(usize::MAX),
                    len,
                }));
            }
        }
        // SAFETY: this lease is the only one (checked above, and `&mut self` stops it sharing),
        // so nothing else reads the entry.
        unsafe { *self.slot().entry.get() = entry };
        Ok(())
    }

    /// The per-frame metadata for writing, while this is the only lease.
    pub fn meta_mut(&mut self) -> Option<&mut M> {
        if !self.is_sole_owner() {
            return None;
        }
        // SAFETY: this lease is the only one and `&mut self` stops it sharing while the borrow
        // lives, so nothing else reads the metadata.
        Some(unsafe { &mut *self.slot().meta.get() })
    }

    /// The planes for reading, inside a CPU read window (a dma-buf's sync START, ended when the
    /// last open window on the buffer closes). `None` when the memory is not mapped here.
    pub fn read(&self) -> Option<CpuRead<'_, M>> {
        self.finish_write();
        let slot = self.slot();
        let ptr = slot.memory().cpu_ptr()?;
        slot.readers.begin(slot.memory());
        // SAFETY: `BufferMemory` promises `len` valid bytes at `ptr` while the pool holds the
        // memory, which outlives this borrow of the lease. Writes need `&mut` on the only lease,
        // which this `&self` borrow excludes, so the bytes do not change while the slice lives.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), slot.memory().len()) };
        Some(CpuRead {
            slot,
            bytes,
            layouts: &self.entry().layouts,
        })
    }

    /// The planes for writing, while this is the only lease and the memory is mapped writable.
    /// Opens a CPU write window (a dma-buf's sync START for writing); sharing the frame, reading
    /// it or dropping the lease ends it.
    pub fn write(&mut self) -> Option<PlanesMut<'_>> {
        if !self.is_sole_owner() {
            return None;
        }
        let layouts = self.entry().layouts;
        let slot = self.slot();
        if !slot.memory().cpu_writable() {
            return None;
        }
        let ptr = slot.memory().cpu_ptr()?;
        if !slot.writing.swap(true, Ordering::Relaxed) {
            slot.memory().begin_cpu(CpuAccess::Write);
        }
        // SAFETY: `BufferMemory` promises `len` writable bytes at `ptr` while the pool holds the
        // memory. This is the only lease and `&mut self` is borrowed for the slice's life, so no
        // other slice of these bytes exists or can be made.
        let bytes = unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), slot.memory().len()) };
        Some(PlanesMut { bytes, layouts })
    }

    fn finish_write(&self) {
        let slot = self.slot();
        if slot.writing.swap(false, Ordering::Relaxed) {
            slot.memory().end_cpu(CpuAccess::Write);
        }
    }

    /// Copies this frame's visible rows into `dst` (the only lease on its frame) and gives `dst`
    /// this frame's id, timestamp and format, keeping `dst`'s plane layout. The one copy site in
    /// this crate, counted in [`crate::COPY_INTO`]. Returns the bytes copied.
    pub fn copy_into<N>(&self, dst: &mut FrameLease<N>) -> Result<usize, CopyError> {
        let src = self.read().ok_or(CopyError::NotReadable)?;
        let entry = *self.entry();
        let dst_layouts = *dst.layouts();
        if dst_layouts.len() != entry.layouts.len() {
            return Err(CopyError::PlaneCount);
        }
        let resolution = entry.format.resolution;
        let geometry = match plane_geometry(
            entry.format.code,
            resolution.width.get(),
            resolution.height.get(),
        ) {
            Ok(geometry) => Some(geometry),
            Err(LayoutError::Compressed(_)) => None,
            Err(error) => return Err(CopyError::Layout(error)),
        };
        let mut copied = 0usize;
        {
            let mut out = dst.write().ok_or(CopyError::NotWritable)?;
            for index in 0..entry.layouts.len() {
                let from = src.plane(index).ok_or(CopyError::TooSmall)?;
                let mut to = out.plane(index).ok_or(CopyError::TooSmall)?;
                match geometry.as_ref().and_then(|g| g.get(index)) {
                    Some(shape) => {
                        for row in 0..shape.rows {
                            let a = from
                                .row_bytes(row, shape.row_bytes)
                                .ok_or(CopyError::TooSmall)?;
                            let b = to
                                .row_bytes_mut(row, shape.row_bytes)
                                .ok_or(CopyError::TooSmall)?;
                            b.copy_from_slice(a);
                        }
                        copied += shape.row_bytes * shape.rows;
                    }
                    None => {
                        let data = from.data;
                        to.data
                            .get_mut(..data.len())
                            .ok_or(CopyError::TooSmall)?
                            .copy_from_slice(data);
                        copied += data.len();
                    }
                }
            }
        }
        drop(src);
        let mut moved = entry;
        moved.layouts = dst_layouts;
        dst.set_entry(moved).map_err(|_| CopyError::NotWritable)?;
        COPY_INTO.record(copied);
        Ok(copied)
    }
}

impl<M> Clone for FrameLease<M> {
    fn clone(&self) -> Self {
        self.share()
    }
}

impl<M> Drop for FrameLease<M> {
    fn drop(&mut self) {
        self.finish_write();
        // Release: whoever takes the buffer next (an acquire on `refs`) sees every access made
        // through this lease.
        self.slot().refs.fetch_sub(1, Ordering::Release);
    }
}

impl<M> fmt::Debug for FrameLease<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameLease")
            .field("buffer", &self.index)
            .field("holders", &self.holders())
            .field("entry", self.entry())
            .finish()
    }
}

/// Why [`FrameLease::set_entry`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetError {
    /// Another lease holds the frame.
    Shared,
    TooSmall(AcquireError),
}

/// Why [`FrameLease::copy_into`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    NotReadable,
    /// The destination is shared, not mapped, or read-only.
    NotWritable,
    PlaneCount,
    /// A destination or source plane is shorter than the format's rows.
    TooSmall,
    Layout(LayoutError),
}

impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReadable => f.write_str("source frame is not mapped for reading"),
            Self::NotWritable => f.write_str("destination frame is shared or not writable"),
            Self::PlaneCount => f.write_str("frames have different plane counts"),
            Self::TooSmall => f.write_str("a plane is shorter than its rows"),
            Self::Layout(error) => write!(f, "{error}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CopyError {}

impl fmt::Display for SetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shared => f.write_str("another lease holds the frame"),
            Self::TooSmall(error) => write!(f, "{error}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SetError {}

/// An open CPU read of a frame's planes.
pub struct CpuRead<'a, M> {
    slot: &'a Slot<M>,
    bytes: &'a [u8],
    layouts: &'a Layouts,
}

impl<'a, M> CpuRead<'a, M> {
    pub fn planes(&self) -> usize {
        self.layouts.len()
    }

    /// Plane `index`, `None` past the last.
    pub fn plane(&self, index: usize) -> Option<Plane<'a>> {
        let layout = self.layouts.get(index)?;
        Some(Plane {
            data: self.bytes.get(layout.offset..layout.end()?)?,
            stride: layout.stride,
        })
    }

    /// The whole buffer.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

impl<M> Drop for CpuRead<'_, M> {
    fn drop(&mut self) {
        self.slot.readers.end(self.slot.memory());
    }
}

impl<M> fmt::Debug for CpuRead<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CpuRead")
            .field("planes", &self.planes())
            .finish()
    }
}

/// One plane's bytes, for reading.
#[derive(Debug, Clone, Copy)]
pub struct Plane<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

impl<'a> Plane<'a> {
    /// The first `row_bytes` of row `row`.
    pub fn row_bytes(&self, row: usize, row_bytes: usize) -> Option<&'a [u8]> {
        let start = row.checked_mul(self.stride)?;
        self.data.get(start..start.checked_add(row_bytes)?)
    }
}

/// A frame's planes, for writing.
pub struct PlanesMut<'a> {
    bytes: &'a mut [u8],
    layouts: Layouts,
}

impl PlanesMut<'_> {
    pub fn planes(&self) -> usize {
        self.layouts.len()
    }

    /// Plane `index`, `None` past the last.
    pub fn plane(&mut self, index: usize) -> Option<PlaneMut<'_>> {
        let layout: PlaneLayout = *self.layouts.get(index)?;
        Some(PlaneMut {
            data: self.bytes.get_mut(layout.offset..layout.end()?)?,
            stride: layout.stride,
        })
    }

    /// The whole buffer.
    pub fn bytes(&mut self) -> &mut [u8] {
        self.bytes
    }
}

impl fmt::Debug for PlanesMut<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanesMut")
            .field("layouts", &self.layouts)
            .finish()
    }
}

/// One plane's bytes, for writing.
#[derive(Debug)]
pub struct PlaneMut<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
}

impl PlaneMut<'_> {
    /// The first `row_bytes` of row `row`.
    pub fn row_bytes_mut(&mut self, row: usize, row_bytes: usize) -> Option<&mut [u8]> {
        let start = row.checked_mul(self.stride)?;
        self.data.get_mut(start..start.checked_add(row_bytes)?)
    }
}
