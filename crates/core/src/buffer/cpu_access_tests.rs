//! CPU readability follows what the backing says, not where the memory lives.

use std::sync::Arc;

use crate::prelude::*;

/// A dma-buf mapped for the CPU, as cached or uncached as it says.
struct MappedDmabuf {
    plane: Vec<u8>,
    access: CpuAccess,
}

impl ExternalBacking for MappedDmabuf {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        (index == 0).then_some(self.plane.as_slice())
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::Dmabuf
    }

    fn cpu_access(&self) -> CpuAccess {
        self.access
    }
}

/// A dma-buf backing that does not say whether it is mapped.
struct SilentDmabuf;

impl ExternalBacking for SilentDmabuf {
    fn plane_data(&self, _index: usize) -> Option<&[u8]> {
        None
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::Dmabuf
    }
}

#[test]
fn cpu_readability_follows_the_backing_not_the_residency() {
    let res = Resolution::new(4, 2).unwrap();
    let meta = FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), 1);
    let layout = || {
        smallvec::smallvec![PlaneLayout {
            offset: 0,
            len: 8,
            stride: 4,
        }]
    };
    let dmabuf = |access| {
        FrameLease::from_external(
            meta.clone(),
            layout(),
            Arc::new(MappedDmabuf {
                plane: vec![9; 8],
                access,
            }),
        )
    };

    // A mapped, cached dma-buf reads like host memory, though it lives in a dma-buf.
    let cached = dmabuf(CpuAccess::Cached);
    assert_eq!(cached.residency(), FrameResidency::Dmabuf);
    assert_eq!(cached.cpu_access(), CpuAccess::Cached);
    assert!(cached.can_read_planes() && cached.require_host_readable().is_ok());
    assert_eq!(cached.to_visible_vec().unwrap(), vec![9; 8]);
    // Uncached: readable, at a cost.
    assert!(dmabuf(CpuAccess::Uncached).can_read_planes());
    // A dma-buf backing that says nothing is not readable.
    let unknown = FrameLease::from_external(meta.clone(), layout(), Arc::new(SilentDmabuf));
    assert_eq!(unknown.cpu_access(), CpuAccess::None);
    assert!(!unknown.can_read_planes());
    // Frames in their own buffers are cached host memory; GPU textures are never readable.
    let mut owned = BufferPool::with_limits(1, 8, 1).lease();
    owned.resize(8);
    let owned = FrameLease::single_plane(meta.clone(), owned, 8, 4);
    assert_eq!(owned.cpu_access(), CpuAccess::Cached);
    let mut gpu = dmabuf(CpuAccess::Cached);
    gpu.meta_mut().residency = Some(FrameResidency::GpuTexture);
    assert!(!gpu.can_read_planes());
}

mod read_window {
    use core::cell::Cell;

    use crate::buffer::CpuReadWindow;

    /// START and END calls made through `window`.
    struct Syncs {
        starts: Cell<u32>,
        ends: Cell<u32>,
    }

    impl Syncs {
        fn new() -> Self {
            Self {
                starts: Cell::new(0),
                ends: Cell::new(0),
            }
        }
        fn start(&self) {
            self.starts.set(self.starts.get() + 1);
        }
        fn end(&self) {
            self.ends.set(self.ends.get() + 1);
        }
        fn get(&self) -> (u32, u32) {
            (self.starts.get(), self.ends.get())
        }
    }

    #[test]
    fn overlapping_bracketed_reads_sync_on_the_first_begin_and_the_last_end() {
        let (window, syncs) = (CpuReadWindow::new(), Syncs::new());
        window.begin(|| syncs.start());
        window.begin(|| syncs.start());
        assert_eq!(syncs.get(), (1, 0));
        window.end(|| syncs.end());
        assert_eq!(syncs.get(), (1, 0));
        window.end(|| syncs.end());
        assert_eq!(syncs.get(), (1, 1));
        assert!(!window.is_open());
        // An end without a begin does nothing; the next read syncs again.
        window.end(|| syncs.end());
        window.begin(|| syncs.start());
        window.end(|| syncs.end());
        assert_eq!(syncs.get(), (2, 2));
        window.close(|| syncs.end());
        assert_eq!(syncs.get(), (2, 2));
    }

    #[test]
    fn a_held_read_keeps_the_window_open_until_close() {
        let (window, syncs) = (CpuReadWindow::new(), Syncs::new());
        // Held inside a bracketed read: no second START, and the bracket's end leaves it open.
        window.begin(|| syncs.start());
        window.hold(|| syncs.start());
        window.end(|| syncs.end());
        assert_eq!(syncs.get(), (1, 0));
        // Brackets inside a held window sync nothing.
        window.begin(|| syncs.start());
        window.end(|| syncs.end());
        window.hold(|| syncs.start());
        assert_eq!(syncs.get(), (1, 0));
        window.close(|| syncs.end());
        assert_eq!(syncs.get(), (1, 1));
        // Never read: nothing to end.
        let (window, syncs) = (CpuReadWindow::new(), Syncs::new());
        window.close(|| syncs.end());
        assert_eq!(syncs.get(), (0, 0));
    }
}
