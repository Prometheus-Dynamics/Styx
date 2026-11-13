//! The read loop allocates nothing per frame, in steady state: the reader takes frames from the
//! service in this process, with and without the soft ISP, and the allocations this thread makes
//! while the loop runs are counted. Other threads (the service's) are not counted.
//!
//! A reader that keeps up sees every frame: no gap, with and without the soft ISP, as long as the
//! ISP's time per frame is below (keep - 1) frame periods (the window the service keeps for it).

#[path = "support/mod.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use styx_measure::cli::parse_fps_milli;
use styx_measure::control::CameraLine;
use styx_measure::isp::{IspRunner, SoftIspArg};
use styx_measure::session::Session;
use styx_service::Service;
use support::{CAMERA, daemon, list, socket_of, start_service, test_dir, wait_publishing};

/// The camera's keep window, from the service's status.
fn keep_of(service: &Service) -> u64 {
    let lines = list(service);
    let line = lines
        .iter()
        .filter_map(|l| CameraLine::parse(l))
        .find(|c| c.id == CAMERA)
        .expect("the camera is listed");
    line.number("keep").expect("keep in the status")
}

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
    // One frame each 8 333 333 ns at 120 fps; the keep window is read from the status.
    let period_ns = 1e9 / 120.0;
    let keep = keep_of(&service);

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
            "steady state (isp {isp:?}): 1000 frames, {made} allocations on the read loop, {} gaps, keep {keep}",
            session.stats.gaps
        );
        assert_eq!(
            made, 0,
            "the read loop made {made} allocations for 1000 frames"
        );
        // The claim: no gap and no skip for a reader that keeps up. With the ISP it applies while
        // the ISP's time per frame is below (keep - 1) frame periods.
        let bound_ns = (keep as f64 - 1.0) * period_ns;
        let isp_max = session
            .isp_stats()
            .and_then(|s| s.times.max())
            .map(|ns| ns as f64);
        let within_bound = isp.is_none() || isp_max.is_some_and(|m| m < bound_ns);
        // "Keeps up" is checked, not assumed: the longest capture-to-reader time covers a stall
        // of the reader, the service or the simulated camera (a busy machine).
        let latency_max = session.stats.latency.max().map(|ns| ns as f64);
        if latency_max.is_some_and(|m| m >= bound_ns) {
            println!(
                "isp {isp:?}: capture-to-reader max {latency_max:?} ns is not below {bound_ns} ns: a process was held up; zero gaps not asserted ({} gaps)",
                session.stats.gaps
            );
        } else if within_bound {
            assert_eq!(session.stats.gaps, 0, "a reader that keeps up has no gaps");
            assert_eq!(
                session.reader_stats().skipped,
                0,
                "the ring skipped no frame"
            );
        } else {
            println!(
                "isp {isp:?}: soft ISP max {isp_max:?} ns is not below {bound_ns} ns; zero gaps not asserted ({} gaps)",
                session.stats.gaps
            );
        }
        assert!(stop.is_some());
    }
    service.shutdown();
}
