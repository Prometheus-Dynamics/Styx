//! `FrameLease::descriptor` is called on hot paths (argument checks in image operations): it must
//! not allocate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use styx_core::prelude::*;

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn descriptor_does_not_allocate() {
    let res = Resolution::new(64, 48).unwrap();
    let pool = BufferPool::with_limits(2, 64 * 48, 2);
    let (mut luma, mut chroma) = (pool.lease(), pool.lease());
    luma.resize(64 * 48);
    chroma.resize(64 * 24);
    let frame = FrameLease::multi_plane(
        FrameMeta::new(MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709), 7),
        smallvec::smallvec![luma, chroma],
        smallvec::smallvec![
            PlaneLayout {
                offset: 0,
                len: 64 * 48,
                stride: 64,
            },
            PlaneLayout {
                offset: 0,
                len: 64 * 24,
                stride: 64,
            },
        ],
    );
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..100 {
        let descriptor = std::hint::black_box(frame.descriptor());
        assert_eq!(descriptor.planes.len(), 2);
    }
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed) - before, 0);
}
