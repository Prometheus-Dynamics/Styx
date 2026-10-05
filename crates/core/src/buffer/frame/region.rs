//! Frames over caller-provided memory: a static buffer, or a region a DMA engine filled (or will
//! read).
//!
//! A firmware's receiver writes frames into memory the firmware placed (a linker section, a
//! `StaticDma` carve-out, an SRAM bank); [`MemoryRegion`] lends that memory to a
//! [`FrameLease`] without copying. [`RegionHooks`] are the moments the platform cares about:
//! the CPU's first read (a D-cache invalidate on a Cortex-M7, nothing on coherent memory), the
//! CPU's writes to a writable region (its output: begin, then end with a D-cache clean so a
//! device reads them), and the last view of the frame going away (queue the buffer to the
//! receiver again, or to the device that reads it).

use core::ptr::NonNull;

use smallvec::SmallVec;

use super::{ExternalBacking, FrameLease, FrameMeta, PlaneLayout, shared_backing};
use crate::buffer::cpu_access::CpuAccess;
use crate::buffer::meta::FrameResidency;
use crate::sync::{AtomicBool, Ordering};

/// What the platform does around a [`MemoryRegion`]'s life as a frame. All default to nothing.
///
/// In order: [`begin_cpu_read`](RegionHooks::begin_cpu_read) before the CPU first reads the
/// region; for a writable region, [`begin_cpu_write`](RegionHooks::begin_cpu_write) before the
/// frame's first write and [`end_cpu_write`](RegionHooks::end_cpu_write) once the writes are
/// done (the frame is shared, its backing handed out, [`FrameLease::finish_cpu_write`], or the
/// region dropped), possibly again for a later write; and [`release`](RegionHooks::release)
/// last. On Linux a dma-buf's hooks are its `DMA_BUF_IOCTL_SYNC` pairs
/// (`dmabuf_begin_cpu_read` / `dmabuf_end_cpu_read`, `dmabuf_begin_cpu_write` /
/// `dmabuf_end_cpu_write` with `std`).
pub trait RegionHooks: Send + Sync + 'static {
    /// Before the CPU first reads the region (once per region): cache maintenance for memory a
    /// device wrote, e.g. a D-cache invalidate by address on a Cortex-M7. Not called once a
    /// write window has begun (the CPU's own writes are what it reads then).
    fn begin_cpu_read(&self, _bytes: &[u8]) {}

    /// Before the CPU first writes a writable region (and reads it, until
    /// [`RegionHooks::end_cpu_write`]): e.g. a dma-buf sync START for reading and writing.
    /// Nothing is needed on a Cortex-M7 for memory only the CPU writes meanwhile.
    fn begin_cpu_write(&self, _bytes: &[u8]) {}

    /// The CPU's writes are done: make them visible to devices, e.g. a D-cache clean by address
    /// on a Cortex-M7 or a dma-buf sync END. Called once per [`RegionHooks::begin_cpu_write`],
    /// and before [`RegionHooks::release`].
    fn end_cpu_write(&self, _bytes: &[u8]) {}

    /// The last view of the frame was dropped: the memory is the platform's again (e.g. queue
    /// the buffer to the receiver).
    fn release(&self) {}
}

/// No cache maintenance, nothing on release (static or coherent memory the caller keeps).
impl RegionHooks for () {}

/// Caller-provided memory as a frame backing ([`FrameLease::from_region`]). Every plane is
/// read from the one region at its layout's offset. A writable region
/// ([`MemoryRegion::from_raw_mut`], [`MemoryRegion::from_static_mut`]) is also written in place
/// by the frame while it is the region's one owner ([`FrameLease::planes_mut`],
/// [`FrameLease::plane_data_mut`], [`FrameLease::visible_rows_mut`]).
pub struct MemoryRegion<H: RegionHooks = ()> {
    ptr: NonNull<u8>,
    len: usize,
    hooks: H,
    cpu_access: CpuAccess,
    residency: FrameResidency,
    writable: bool,
    begun: AtomicBool,
    writing: AtomicBool,
}

// SAFETY: the region is read through shared slices, and written only through `&mut self`
// (`bytes_mut`), which excludes every other access; whoever built it promised (in `from_raw` /
// `from_raw_mut`) that nothing else touches it while it lives. The other state is atomics.
unsafe impl<H: RegionHooks> Send for MemoryRegion<H> {}
// SAFETY: as above.
unsafe impl<H: RegionHooks> Sync for MemoryRegion<H> {}

impl MemoryRegion<()> {
    /// A static buffer (flash, a `static` array, leaked memory), read-only.
    pub fn from_static(bytes: &'static [u8]) -> Self {
        // SAFETY: a `'static` shared slice is valid and never written for the program's life.
        unsafe { Self::from_raw(bytes.as_ptr(), bytes.len(), ()) }
    }

    /// A static buffer the frame may also write (a `static mut` carve-out, leaked memory).
    pub fn from_static_mut(bytes: &'static mut [u8]) -> Self {
        // SAFETY: a `'static` exclusive slice is valid for the program's life and, moved in
        // here, reached through nothing else.
        unsafe { Self::from_raw_mut(bytes.as_mut_ptr(), bytes.len(), ()) }
    }
}

impl<H: RegionHooks> MemoryRegion<H> {
    /// `len` bytes at `ptr`, with `hooks`, read-only.
    ///
    /// # Safety
    /// The bytes must stay valid, and nothing (CPU or device) may write them, until the region
    /// is dropped (after the last frame over it, when [`RegionHooks::release`] runs).
    pub unsafe fn from_raw(ptr: *const u8, len: usize, hooks: H) -> Self {
        Self {
            ptr: NonNull::new(ptr as *mut u8).unwrap_or(NonNull::dangling()),
            len: if ptr.is_null() { 0 } else { len },
            hooks,
            cpu_access: CpuAccess::Cached,
            residency: FrameResidency::HostExternal,
            writable: false,
            begun: AtomicBool::new(false),
            writing: AtomicBool::new(false),
        }
    }

    /// `len` bytes at `ptr`, with `hooks`, which a frame over the region may also write
    /// (an output buffer: a frame an operation fills, a buffer a device then reads).
    ///
    /// # Safety
    /// The bytes must stay valid for reads and writes, and nothing else (CPU or device) may
    /// read or write them, until the region is dropped (after the last frame over it, when
    /// [`RegionHooks::release`] runs).
    pub unsafe fn from_raw_mut(ptr: *mut u8, len: usize, hooks: H) -> Self {
        // SAFETY: the caller's promise covers `from_raw`'s.
        let mut region = unsafe { Self::from_raw(ptr, len, hooks) };
        // `from_raw` kept `ptr` (with its write permission) unless it was null.
        region.writable = !ptr.is_null();
        region
    }

    /// How the CPU reads the region (default [`CpuAccess::Cached`]; uncached SRAM or a
    /// write-combined mapping says so, device-only memory says [`CpuAccess::None`], which also
    /// makes a writable region unwritable by the CPU).
    pub fn with_cpu_access(mut self, access: CpuAccess) -> Self {
        self.cpu_access = access;
        self
    }

    /// Where the memory lives (default [`FrameResidency::HostExternal`]).
    pub fn with_residency(mut self, residency: FrameResidency) -> Self {
        self.residency = residency;
        self
    }

    /// The hooks.
    pub fn hooks(&self) -> &H {
        &self.hooks
    }

    /// Whether the CPU may write the region ([`MemoryRegion::from_raw_mut`] and CPU access).
    pub fn is_writable(&self) -> bool {
        self.writable && self.cpu_access.readable()
    }

    fn slice(&self) -> &[u8] {
        // SAFETY: valid for the region's life (`from_raw`); no `&mut` to it exists while `self`
        // is borrowed shared (`bytes_mut` takes `&mut self`).
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The region's bytes, starting CPU access on the first call.
    pub fn bytes(&self) -> &[u8] {
        let bytes = self.slice();
        if !self.begun.load(Ordering::Acquire) && !self.begun.swap(true, Ordering::AcqRel) {
            self.hooks.begin_cpu_read(bytes);
        }
        bytes
    }

    /// The region's bytes for writing, or `None` when it is not writable
    /// ([`MemoryRegion::is_writable`]). The first call (and the first after
    /// [`MemoryRegion::finish_cpu_write`]) runs [`RegionHooks::begin_cpu_write`].
    pub fn bytes_mut(&mut self) -> Option<&mut [u8]> {
        if !self.is_writable() {
            return None;
        }
        if !*self.writing.get_mut() {
            *self.writing.get_mut() = true;
            // The CPU's own writes are what it reads from now: no read invalidate later.
            *self.begun.get_mut() = true;
            self.hooks.begin_cpu_write(self.slice());
        }
        // SAFETY: valid for reads and writes for the region's life (`from_raw_mut`: `writable`
        // is only set there, with a non-null pointer that has write permission), and
        // `&mut self` excludes every other slice of it for the borrow.
        Some(unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) })
    }

    /// Ends an open write window: [`RegionHooks::end_cpu_write`]. Nothing when none is open.
    pub fn finish_cpu_write(&self) {
        if self.writing.load(Ordering::Acquire) && self.writing.swap(false, Ordering::AcqRel) {
            self.hooks.end_cpu_write(self.slice());
        }
    }
}

impl<H: RegionHooks> Drop for MemoryRegion<H> {
    fn drop(&mut self) {
        self.finish_cpu_write();
        self.hooks.release();
    }
}

impl<H: RegionHooks> ExternalBacking for MemoryRegion<H> {
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        self.cpu_access.readable().then(|| self.bytes())
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.len)
    }

    fn backing_kind(&self) -> &'static str {
        "region"
    }

    fn residency(&self) -> FrameResidency {
        self.residency
    }

    fn cpu_access(&self) -> CpuAccess {
        self.cpu_access
    }

    fn cpu_writable(&self) -> bool {
        self.is_writable()
    }

    fn plane_data_mut(&mut self, _index: usize) -> Option<&mut [u8]> {
        self.bytes_mut()
    }

    fn finish_cpu_write(&self) {
        MemoryRegion::finish_cpu_write(self);
    }
}

impl FrameLease {
    /// A frame over caller-provided memory, without copying: the planes at their layouts'
    /// offsets in `region`. Read-only unless the region is writable
    /// ([`MemoryRegion::from_raw_mut`]): then the frame writes the planes in place while it is
    /// the region's one owner (before [`FrameLease::share`] or
    /// [`FrameLease::external_backing_handle`]). The region (and its [`RegionHooks::release`])
    /// lives until the last view of the frame is dropped.
    pub fn from_region<H: RegionHooks>(
        meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        region: MemoryRegion<H>,
    ) -> Self {
        let mut meta = meta;
        if meta.residency.is_none() {
            meta.residency = Some(region.residency);
        }
        Self::from_external(meta, layouts, shared_backing(region))
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod write_tests;
