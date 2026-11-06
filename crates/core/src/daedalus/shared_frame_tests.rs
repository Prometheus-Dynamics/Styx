use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use ::daedalus::transport::{
    DRM_FORMAT_MOD_LINEAR, ForeignHandle, FrameColorimetry, FrameFormatKind, FramePlane,
    FrameResidency as ViewResidency, FrameSource, SharedFrame, fourcc,
};

use super::*;
use crate::buffer::{CpuAccess, FrameLease, FrameMutability, FrameResidency};
use crate::format::{ColorSpace, FourCc};

/// A frame another build made: NV12 in host memory, counting its releases and its open CPU
/// reads.
struct Foreign {
    luma: Vec<u8>,
    chroma: Vec<u8>,
    released: Arc<AtomicUsize>,
    open_reads: Arc<AtomicI32>,
}

impl Foreign {
    fn new(released: &Arc<AtomicUsize>, open_reads: &Arc<AtomicI32>) -> Arc<Self> {
        Arc::new(Self {
            luma: (0..64u32 * 32).map(|i| i as u8).collect(),
            chroma: vec![128; 64 * 16],
            released: Arc::clone(released),
            open_reads: Arc::clone(open_reads),
        })
    }
}

impl Drop for Foreign {
    fn drop(&mut self) {
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}

impl FrameSource for Foreign {
    fn width(&self) -> u32 {
        64
    }

    fn height(&self) -> u32 {
        32
    }

    fn format(&self) -> u32 {
        fourcc(b"NV12")
    }

    fn format_kind(&self) -> FrameFormatKind {
        FrameFormatKind::Pixel
    }

    fn modifier(&self) -> u64 {
        DRM_FORMAT_MOD_LINEAR
    }

    fn timestamp_ns(&self) -> u64 {
        1_000
    }

    fn sequence(&self) -> u64 {
        7
    }

    fn residency(&self) -> ViewResidency {
        ViewResidency::Cpu
    }

    fn plane_count(&self) -> u32 {
        2
    }

    fn plane(&self, index: u32) -> Option<FramePlane> {
        Some(match index {
            0 => FramePlane::cpu(&self.luma, 64),
            1 => FramePlane::cpu(&self.chroma, 64),
            _ => return None,
        })
    }

    fn plane_data(&self, index: u32) -> Option<&[u8]> {
        let bytes = match index {
            0 => &self.luma[..],
            1 => &self.chroma[..],
            _ => return None,
        };
        self.open_reads.fetch_add(1, Ordering::SeqCst);
        Some(bytes)
    }

    fn end_cpu_access(&self, _index: u32) {
        self.open_reads.fetch_sub(1, Ordering::SeqCst);
    }

    fn colorimetry(&self) -> FrameColorimetry {
        FrameColorimetry::BT709
    }
}

fn counters() -> (Arc<AtomicUsize>, Arc<AtomicI32>) {
    (Arc::default(), Arc::default())
}

/// As a frame from a separately built plugin arrives: a handle, not this build's own.
fn received(frame: Arc<Foreign>) -> SharedFrame {
    let handle: ForeignHandle = SharedFrame::new(frame).into_handle();
    SharedFrame::from_handle(handle).unwrap()
}

#[test]
fn a_foreign_frame_is_a_lease_over_its_own_memory() {
    let (released, reads) = counters();
    let source = Foreign::new(&released, &reads);
    let luma = source.luma.as_ptr();
    let frame = received(source);
    let lease = from_shared_frame(&frame).unwrap();
    let meta = lease.meta();
    assert_eq!(meta.format.code, FourCc::NV12);
    assert_eq!(meta.format.resolution.width.get(), 64);
    assert_eq!(meta.format.color, ColorSpace::Bt709);
    assert_eq!((meta.timestamp, meta.sequence()), (1_000, Some(7)));
    assert_eq!(lease.residency(), FrameResidency::HostExternal);
    assert_eq!(lease.cpu_access(), CpuAccess::Cached);
    assert_eq!(meta.mutability, FrameMutability::ReadOnly);
    // In place: the same bytes, no copy.
    let planes = lease.planes();
    assert_eq!(planes.len(), 2);
    assert_eq!(planes[0].data().as_ptr(), luma);
    assert_eq!(
        (planes[0].data().len(), planes[1].data().len()),
        (2048, 1024)
    );
    // Bracketed reads pass through and balance.
    let before = reads.load(Ordering::SeqCst);
    assert!(lease.begin_cpu_read(1).is_some());
    lease.end_cpu_read(1);
    assert_eq!(reads.load(Ordering::SeqCst), before);
    drop(planes);
    drop((lease, frame));
    assert_eq!(released.load(Ordering::SeqCst), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 0, "a CPU access left open");
}

#[test]
fn the_same_frame_gives_the_same_lease_while_it_lives() {
    let (released, reads) = counters();
    let frame = received(Foreign::new(&released, &reads));
    let a = from_shared_frame(&frame).unwrap();
    let b = from_shared_frame(&frame.clone()).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    let weak = Arc::downgrade(&a);
    drop((a, b));
    assert!(weak.upgrade().is_none());
    // Gone: the same frame gets a new lease; another frame never gets this one.
    let c = from_shared_frame(&frame).unwrap();
    assert!(Arc::ptr_eq(&c, &from_shared_frame(&frame).unwrap()));
    let other = received(Foreign::new(&released, &reads));
    assert!(!Arc::ptr_eq(&c, &from_shared_frame(&other).unwrap()));
}

#[test]
fn the_frame_is_released_once_whoever_drops_last() {
    // The node drops the lease while the frame lives on.
    let (released, reads) = counters();
    let frame = received(Foreign::new(&released, &reads));
    drop(from_shared_frame(&frame).unwrap());
    assert_eq!(released.load(Ordering::SeqCst), 0);
    drop(frame);
    assert_eq!(released.load(Ordering::SeqCst), 1);

    // The node keeps the lease in its state past the frame's handle.
    let (released, reads) = counters();
    let frame = received(Foreign::new(&released, &reads));
    let kept = from_shared_frame(&frame).unwrap();
    drop(frame);
    assert_eq!(released.load(Ordering::SeqCst), 0);
    assert_eq!(kept.planes()[0].data()[5], 5);
    drop(kept);
    assert_eq!(released.load(Ordering::SeqCst), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    // The handle comes back through another round of a plugin boundary (as after a reload
    // that hands the frame on): one frame, one release.
    let (released, reads) = counters();
    let frame = received(Foreign::new(&released, &reads));
    let lease = from_shared_frame(&frame).unwrap();
    let again = SharedFrame::from_handle(frame.into_handle()).unwrap();
    assert!(Arc::ptr_eq(&lease, &from_shared_frame(&again).unwrap()));
    drop(again);
    drop(lease);
    assert_eq!(released.load(Ordering::SeqCst), 1);
}

#[test]
fn a_styx_frame_comes_back_as_itself() {
    let lease = Arc::new(super::tests::grey(16, 8, 42));
    // Sent into a graph shared; a node's `SharedFrame` is a handle on the same lease.
    let payload = shared_frame_payload(Arc::clone(&lease));
    let frame = SharedFrame::new(payload.get_arc::<FrameLease>().unwrap());
    let back = from_shared_frame(&frame).unwrap();
    assert!(Arc::ptr_eq(&back, &lease));
    drop((frame, payload, back));
    assert_eq!(Arc::strong_count(&lease), 1);
}

#[test]
fn unknown_formats_and_plane_counts_are_errors() {
    struct Odd;
    impl FrameSource for Odd {
        fn width(&self) -> u32 {
            4
        }
        fn height(&self) -> u32 {
            4
        }
        fn format(&self) -> u32 {
            fourcc(b"ZZZZ")
        }
        fn format_kind(&self) -> FrameFormatKind {
            FrameFormatKind::Pixel
        }
        fn residency(&self) -> ViewResidency {
            ViewResidency::Gpu
        }
        fn plane_count(&self) -> u32 {
            0
        }
        fn plane(&self, _: u32) -> Option<FramePlane> {
            None
        }
    }
    let frame = SharedFrame::new(Arc::new(Odd));
    assert!(matches!(
        from_shared_frame(&frame),
        Err(SharedFrameError::UnknownFormat { .. })
    ));
}

/// One foreign frame received 1000 times: while its lease lives every receipt finds it (no new
/// entry), and once the lease drops the next receipt sweeps the dead entry before adding one, so
/// the frame never has more than one entry and its lease no reference.
#[test]
fn a_foreign_frame_received_again_and_again_has_one_entry() {
    let (released, reads) = counters();
    let frame = received(Foreign::new(&released, &reads));
    let key = frame.handle().data() as usize;
    let kept = from_shared_frame(&frame).unwrap();
    for _ in 0..1000 {
        let again = from_shared_frame(&frame).unwrap();
        assert!(Arc::ptr_eq(&again, &kept));
        assert_eq!(super::shared_frame::entries_for(key), 1);
    }
    drop(kept);
    for i in 0..1000 {
        let lease = from_shared_frame(&frame).unwrap();
        assert_eq!(super::shared_frame::entries_for(key), 1, "receipt {i}");
        assert_eq!(Arc::weak_count(&lease), 1, "only the list's own Weak");
        drop(lease);
    }
    drop(frame);
    assert_eq!(released.load(Ordering::SeqCst), 1);
}
