//! The read loop allocates nothing per frame, in steady state: the reader takes frames from the
//! service in this process, with and without the soft ISP, and the allocations this thread makes
//! while the loop runs are counted. Other threads (the service's) are not counted.

#[path = "support/mod.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use styx_measure::cli::parse_fps_milli;
use styx_measure::isp::{IspRunner, SoftIspArg};
use styx_measure::session::Session;
use support::{daemon, socket_of, start_service, test_dir, wait_publishing};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        }
    });
}

struct Counting;

// SAFETY: every call forwards to the system allocator with the caller's arguments. The only
// addition is a thread-local counter, which is const-initialised and does not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds GlobalAlloc's contract for `layout`; forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with `layout`; forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: `ptr`, `layout` and `new_size` are the caller's; forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Runs `f` with this thread's allocations counted, and returns how many it made.
fn counted(f: impl FnOnce()) -> u64 {
    ALLOCATIONS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn the_read_loop_allocates_nothing_per_frame() {
    let dir = test_dir("steady");
    let lemnos = dir.join("lemnos.sock");
    let _daemon = daemon(&lemnos);
    let service = start_service(&dir, &lemnos);
    wait_publishing(&service);
    let socket = socket_of(&dir);
    let _ = parse_fps_milli("120.00").expect("the rate parses");

    for isp in [None, Some("NV12")] {
        let arg = isp.map(|c| c.parse::<SoftIspArg>().expect("arg"));
        let mut session =
            Session::connect(&socket, arg.clone().map(IspRunner::new)).expect("connect");
        // Warm-up: the first frames make the ISP's stage and pool, and fill the reader's tables.
        session.run(None, |s| s.delivered >= 200).expect("warm-up");
        let mut stop = None;
        let made = counted(|| {
            stop = Some(
                session
                    .run(None, |s| s.delivered >= 1200)
                    .expect("steady state"),
            );
        });
        assert_eq!(session.stats.delivered, 1200);
        println!(
            "steady state (isp {isp:?}): 1000 frames, {made} allocations on the read loop, {} gaps",
            session.stats.gaps
        );
        assert_eq!(
            made, 0,
            "the read loop made {made} allocations for 1000 frames"
        );
        assert!(stop.is_some());
    }
    service.shutdown();
}
