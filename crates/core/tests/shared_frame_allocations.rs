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
