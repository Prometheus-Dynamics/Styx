//! Frames over caller-provided memory: a static buffer, or a region a DMA engine filled.
//!
//! A firmware's receiver writes frames into memory the firmware placed (a linker section, a
//! `StaticDma` carve-out, an SRAM bank); [`MemoryRegion`] lends that memory to a
//! [`FrameLease`] without copying. [`RegionHooks`] are the two moments the platform cares
//! about: the CPU's first read (a D-cache invalidate on a Cortex-M7, nothing on coherent
//! memory) and the last view of the frame going away (queue the buffer to the receiver again).

use core::ptr::NonNull;

use smallvec::SmallVec;

use super::{ExternalBacking, FrameLease, FrameMeta, PlaneLayout, shared_backing};
use crate::buffer::cpu_access::CpuAccess;
use crate::buffer::meta::FrameResidency;
use crate::sync::{AtomicBool, Ordering};

/// What the platform does around a [`MemoryRegion`]'s life as a frame. Both default to
/// nothing.
pub trait RegionHooks: Send + Sync + 'static {
    /// Before the CPU first reads the region (once per region): cache maintenance for memory a
    /// device wrote, e.g. a D-cache invalidate by address on a Cortex-M7.
    fn begin_cpu_read(&self, _bytes: &[u8]) {}

    /// The last view of the frame was dropped: the memory is the platform's again (e.g. queue
    /// the buffer to the receiver).
    fn release(&self) {}
}

/// No cache maintenance, nothing on release (static or coherent memory the caller keeps).
impl RegionHooks for () {}

/// Caller-provided memory as a frame backing ([`FrameLease::from_region`]). Every plane is
/// read from the one region at its layout's offset.
pub struct MemoryRegion<H: RegionHooks = ()> {
    ptr: NonNull<u8>,
    len: usize,
    hooks: H,
    cpu_access: CpuAccess,
    residency: FrameResidency,
    begun: AtomicBool,
}

// SAFETY: the region is only read through shared slices; whoever built it promised (in
// `from_raw`) that nothing writes it while it lives, and `&'static [u8]` is `Sync` itself.
unsafe impl<H: RegionHooks> Send for MemoryRegion<H> {}
// SAFETY: as above; the one mutable state is an atomic.
unsafe impl<H: RegionHooks> Sync for MemoryRegion<H> {}

impl MemoryRegion<()> {
    /// A static buffer (flash, a `static` array, leaked memory).
    pub fn from_static(bytes: &'static [u8]) -> Self {
        // SAFETY: a `'static` shared slice is valid and never written for the program's life.
        unsafe { Self::from_raw(bytes.as_ptr(), bytes.len(), ()) }
    }
}

impl<H: RegionHooks> MemoryRegion<H> {
    /// `len` bytes at `ptr`, with `hooks`.
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
            begun: AtomicBool::new(false),
        }
    }

    /// How the CPU reads the region (default [`CpuAccess::Cached`]; uncached SRAM or a
    /// write-combined mapping says so, device-only memory says [`CpuAccess::None`]).
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

    /// The region's bytes, starting CPU access on the first call.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: valid and unwritten for the region's life (`from_raw`).
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) };
        if !self.begun.load(Ordering::Acquire) && !self.begun.swap(true, Ordering::AcqRel) {
            self.hooks.begin_cpu_read(bytes);
        }
        bytes
    }
}

impl<H: RegionHooks> Drop for MemoryRegion<H> {
    fn drop(&mut self) {
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
}

impl FrameLease {
    /// A read-only frame over caller-provided memory, without copying: the planes at their
    /// layouts' offsets in `region`. The region (and its [`RegionHooks::release`]) lives until
    /// the last view of the frame is dropped.
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
mod tests {
    use super::*;
    use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};
    use crate::sync::{Arc, AtomicUsize};
    use smallvec::smallvec;

    static PIXELS: [u8; 32] = {
        let mut p = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            p[i] = i as u8;
            i += 1;
        }
        p
    };

    fn meta(w: u32, h: u32) -> FrameMeta {
        let format = MediaFormat::new(
            FourCc::GREY,
            Resolution::new(w, h).unwrap(),
            ColorSpace::Unknown,
        );
        FrameMeta::new(format, 5)
    }

    struct Counting {
        reads: Arc<AtomicUsize>,
        released: Arc<AtomicUsize>,
    }

    impl RegionHooks for Counting {
        fn begin_cpu_read(&self, bytes: &[u8]) {
            assert_eq!(bytes.len(), 32);
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
        fn release(&self) {
            self.released.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn static_region_frames_read_in_place() {
        let frame = FrameLease::from_region(
            meta(4, 4),
            smallvec![PlaneLayout {
                offset: 8,
                len: 16,
                stride: 4
            }],
            MemoryRegion::from_static(&PIXELS),
        );
        let planes = frame.planes();
        assert_eq!(planes[0].data()[0], 8);
        assert_eq!(planes[0].data().as_ptr(), PIXELS[8..].as_ptr());
        assert_eq!(frame.external_backing_kind(), Some("region"));
        assert!(!frame.can_write_planes());
    }

    #[test]
    fn hooks_run_once_and_release_with_the_last_view() {
        let (reads, released) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let hooks = Counting {
            reads: reads.clone(),
            released: released.clone(),
        };
        // SAFETY: a static, never written.
        let region = unsafe { MemoryRegion::from_raw(PIXELS.as_ptr(), PIXELS.len(), hooks) };
        let frame = FrameLease::from_region(
            meta(4, 2),
            smallvec![
                PlaneLayout {
                    offset: 0,
                    len: 8,
                    stride: 4
                },
                PlaneLayout {
                    offset: 8,
                    len: 8,
                    stride: 4
                }
            ],
            region,
        );
        let view = frame.share().expect("external frames share");
        assert_eq!(frame.planes()[1].data()[0], 8);
        assert_eq!(view.planes()[0].data()[7], 7);
        assert_eq!(reads.load(Ordering::Relaxed), 1, "one begin for the region");
        drop(frame);
        assert_eq!(released.load(Ordering::Relaxed), 0, "a view is still out");
        drop(view);
        assert_eq!(released.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn device_only_regions_are_not_readable() {
        let frame = FrameLease::from_region(
            meta(4, 4),
            smallvec![PlaneLayout {
                offset: 0,
                len: 16,
                stride: 4
            }],
            MemoryRegion::from_static(&PIXELS).with_cpu_access(CpuAccess::None),
        );
        assert!(!frame.has_host_readable_bytes());
        assert!(frame.planes()[0].data().is_empty());
    }
}
