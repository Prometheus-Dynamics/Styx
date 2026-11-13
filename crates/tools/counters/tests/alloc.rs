//! The frame path allocates nothing: a counting allocator sees no allocation across a long run of
//! recorded frames, drops and window takes (the window is the publisher's, and is excluded).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use styx_counters::{CameraCounters, DropReason};

struct Counting;

// This thread's allocations only: the test harness's own threads allocate while the test runs.
thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

// SAFETY: every call forwards to the system allocator unchanged; it only counts this thread's allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract; forwarded unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this `layout` (forwarded unchanged).
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: forwarded unchanged; `ptr` and `layout` come from this allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn the_frame_path_makes_no_allocation() {
    let counters = CameraCounters::new();
    let before = allocations();
    let mut id = 0_u64;
    for step in 0..200_000_u64 {
        // A gap every 1000 frames; a held drop is recorded for every other one.
        id += if step % 1000 == 999 { 2 } else { 1 };
        if step % 2000 == 1999 {
            counters.record_drop(DropReason::HeldBuffer);
        }
        let ts = 1_000_000 * step;
        counters.record_frame(id, ts, ts + 2_000_000);
    }
    let after = allocations();
    assert_eq!(after - before, 0, "the frame path allocated");
    assert_eq!(counters.frames(), 200_000);
    // The gaps at steps 999, 2999, ... have no drop recorded for them: 100 unattributed.
    assert_eq!(counters.drops(DropReason::Unattributed), 100);
    assert_eq!(counters.drops(DropReason::HeldBuffer), 100);
}
