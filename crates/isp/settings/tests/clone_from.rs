//! Copying a settings value into one kept from before (`clone_from`) allocates nothing once the
//! buffers have grown: the loop's per-frame copy rests on it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

mod common;

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

#[test]
fn clone_from_into_a_kept_value_allocates_nothing() {
    // Two values with the same shape (every option set on both), differing in their numbers.
    let src = common::rich();
    let mut other = common::rich();
    other.black_level = 0.5;
    other.wb[0] = 2.0;
    other.flicker_bands = Some(vec![0.5, 0.75, 1.5]);
    let mut kept = src.clone();
    kept.clone_from(&other);
    kept.clone_from(&src);
    assert_eq!(kept, src);

    let before = allocations();
    for _ in 0..1_000 {
        kept.clone_from(&other);
        kept.clone_from(&src);
    }
    let allocs = allocations() - before;
    assert_eq!(allocs, 0, "allocations in 2,000 in-place copies");
    assert_eq!(kept, src);
}
