//! Read-only regions: in place, hooks once, release with the last view.

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
