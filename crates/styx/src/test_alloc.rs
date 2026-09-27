//! Unit-test allocator that remembers the largest single allocation of each thread, so tests
//! can check that damaged input never makes Styx request huge buffers. (Linux overcommits, so
//! such requests often succeed on a desktop but fail on small devices.)

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Tracking;

thread_local! {
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

fn note(size: usize) {
    // `try_with`: allocations during thread teardown must not touch a destroyed local.
    let _ = LARGEST.try_with(|largest| largest.set(largest.get().max(size)));
}

// SAFETY: forwards to the system allocator unchanged.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static TRACKING: Tracking = Tracking;

/// Run `f` and return its result with the largest single allocation it made on this thread.
#[allow(dead_code)]
pub(crate) fn largest_allocation<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.with(|largest| largest.set(0));
    let value = f();
    (value, LARGEST.with(Cell::get))
}
