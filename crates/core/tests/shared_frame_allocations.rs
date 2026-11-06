//! A Daedalus frame as a lease (`styx_core::daedalus::from_shared_frame`) allocates one thing per
//! new frame, the `Arc<FrameLease>` itself, and nothing for a frame already a lease. The `Arc` is
//! the physical reason: consumers key caches on `Weak<FrameLease>`, and a live `Weak` keeps its
//! allocation, so it cannot come from a pool. The backing record is pooled.
#![cfg(feature = "daedalus")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use daedalus::transport::{
    DRM_FORMAT_MOD_LINEAR, FrameFormatKind, FramePlane, FrameResidency, FrameSource, SharedFrame,
    fourcc,
};
use styx_core::daedalus::from_shared_frame;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator unchanged; the count is a const-initialised
// thread-local, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn counted<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATIONS.with(Cell::get);
    let out = f();
    (out, ALLOCATIONS.with(Cell::get) - before)
}

struct Grey([u8; 64]);

impl FrameSource for Grey {
    fn width(&self) -> u32 {
        8
    }
    fn height(&self) -> u32 {
        8
    }
    fn format(&self) -> u32 {
        fourcc(b"R8  ")
    }
    fn format_kind(&self) -> FrameFormatKind {
        FrameFormatKind::Pixel
    }
    fn modifier(&self) -> u64 {
        DRM_FORMAT_MOD_LINEAR
    }
    fn residency(&self) -> FrameResidency {
        FrameResidency::Cpu
    }
    fn plane_count(&self) -> u32 {
        1
    }
    fn plane(&self, index: u32) -> Option<FramePlane> {
        (index == 0).then(|| FramePlane::cpu(&self.0, 8))
    }
    fn plane_data(&self, index: u32) -> Option<&[u8]> {
        (index == 0).then_some(&self.0[..])
    }
}

#[test]
fn a_new_frame_costs_its_arc_and_a_known_one_nothing() {
    let warm = SharedFrame::new(Arc::new(Grey([0; 64])));
    drop(from_shared_frame(&warm).unwrap());
    for i in 0..50u8 {
        let frame = SharedFrame::new(Arc::new(Grey([i; 64])));
        let (lease, n) = counted(|| from_shared_frame(&frame).unwrap());
        assert_eq!(n, 1, "frame {i}: the Arc<FrameLease> and nothing else");
        let (again, n) = counted(|| from_shared_frame(&frame).unwrap());
        assert_eq!(n, 0, "frame {i}: a frame already a lease");
        assert_eq!(lease.planes()[0].data()[0], i);
        drop((lease, again));
    }
}

/// A pooled Styx frame sent again and again (a producer's carrier reused every frame): each send
/// comes back through a node's `SharedFrame` as the frame's own `Arc`, nothing is kept once the
/// payload drops, so the carrier is unique again (`Arc::get_mut`), and the Styx side of a warm
/// send costs no allocation beyond the payload Daedalus makes.
#[test]
fn a_reused_styx_frame_is_sent_without_keeping_it() {
    use daedalus::transport::{Payload, Residency};
    use styx_core::daedalus::{FRAME_TYPE_KEY, share_frame, shared_frame_payload};
    use styx_core::prelude::*;

    let res = Resolution::new(16, 8).unwrap();
    let meta = FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Bt709), 1);
    let mut buf = BufferPool::with_limits(1, 128, 1).lease();
    buf.resize(128);
    let mut carrier = Arc::new(FrameLease::single_plane(meta, buf, 128, 16));
    let send = |carrier: &Arc<FrameLease>| {
        let payload = shared_frame_payload(Arc::clone(carrier));
        let frame = share_frame(payload.get_arc::<FrameLease>().unwrap());
        let back = from_shared_frame(&frame).unwrap();
        assert!(Arc::ptr_eq(&back, carrier), "the frame's own Arc");
    };
    for _ in 0..3 {
        send(&carrier);
    }
    // What Daedalus itself spends per payload and handle, to subtract.
    let (_, daedalus) = counted(|| {
        let payload = Payload::shared_with(
            FRAME_TYPE_KEY,
            Arc::clone(&carrier),
            Residency::Cpu,
            None,
            Some(128),
        );
        drop(share_frame(payload.get_arc::<FrameLease>().unwrap()));
    });
    for i in 0..1000 {
        let (_, n) = counted(|| send(&carrier));
        assert!(
            n <= daedalus,
            "send {i}: {n} allocations, Daedalus's own {daedalus}"
        );
        assert!(
            Arc::get_mut(&mut carrier).is_some(),
            "send {i}: the carrier is unique again"
        );
    }
    assert_eq!(Arc::weak_count(&carrier), 0);
}
