//! The unit tests' allocator: the system allocator, counting the allocations of a thread while
//! that thread has armed it (`count`). Test only.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    // `try_with`: a thread's locals may be gone while it exits.
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = COUNT.try_with(|n| n.set(n.get() + 1));
        }
    });
}

/// The allocations (and reallocations) `f` makes on this thread.
pub(crate) fn count(f: impl FnOnce()) -> u64 {
    COUNT.with(|n| n.set(0));
    ARMED.with(|armed| armed.set(true));
    f();
    ARMED.with(|armed| armed.set(false));
    COUNT.with(Cell::get)
}

struct Counting;

// SAFETY: every call forwards to the system allocator with the same arguments; the only addition
// is a count in const-initialised thread-locals, which do not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: the caller upholds GlobalAlloc's contract for `layout`; forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` above with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: `ptr` came from `alloc` above with this `layout`; forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;
