//! A warmed-up frame allocates nothing and copies nothing: the raw input is read where it lies,
//! the picture and the statistics are written into buffers the caller and the stage keep.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::*;
use styx_aaa::Statistics;
use styx_format::FourCc;
use styx_frame::{FramePool, copy_counters};
use styx_soft::SoftIspStage;
use styx_soft::params::{Demosaic, YuvMatrix};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards every call to `System` unchanged; it only counts this thread's allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: the caller's contract for `alloc` is passed on as it is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: as for `alloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as for `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

const WARM_UP: u64 = 8;
const FRAMES: u64 = 300;

#[test]
fn a_warm_frame_with_statistics_allocates_and_copies_nothing() {
    let raw_pool = picture_pool(RAW, W, H, 1);
    let raw = raw_lease(&raw_pool, 1);
    let mut stage = SoftIspStage::new(
        RAW,
        W,
        H,
        &settings(true, true),
        options(Demosaic::Bilinear, YuvMatrix::Bt709Limited, true),
    )
    .unwrap();

    // The raw input is read in place: the picture planes start at the lease's own buffer.
    let base = raw.memory().cpu_ptr().unwrap().as_ptr() as usize;
    assert_eq!(
        raw.read().unwrap().plane(0).unwrap().data.as_ptr() as usize,
        base
    );

    let out_pool: FramePool<()> = picture_pool(FourCc::RGB3, W, H, 2);
    let mut stats = Statistics::default();
    let copies_before: Vec<usize> = copy_counters().iter().map(|c| c.copies()).collect();

    let mut before = 0;
    for n in 0..FRAMES {
        if n == WARM_UP {
            before = allocations();
        }
        let mut out = picture(&out_pool, FourCc::RGB3, W, H, 10 + n);
        stage.process(&raw, &mut out, Some(&mut stats)).unwrap();
    }
    let allocated = allocations() - before;
    assert_eq!(
        allocated,
        0,
        "allocations over {} warm frames",
        FRAMES - WARM_UP
    );

    let copies_after: Vec<usize> = copy_counters().iter().map(|c| c.copies()).collect();
    assert_eq!(copies_before, copies_after, "whole-frame copies");
    assert!(!stats.colour.zones.is_empty());
}
