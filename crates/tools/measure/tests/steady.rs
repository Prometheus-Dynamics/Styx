//! The read loop allocates nothing per frame, in steady state: the reader takes frames from the
//! service in this process, with and without the soft ISP, and the allocations this thread makes
//! while the loop runs are counted. Other threads (the service's) are not counted.
//!
//! A reader that keeps up sees every frame the service had for it, with and without the soft ISP:
//! the frame it wants next is younger than (keep - 1) frame periods when it comes back for it (the
//! window the service keeps for it while it has a frame in hand). A frame the service itself never
//! had (its own reader was late for the camera's stream, or its ring had no buffer for it: a busy
//! machine) is a gap the reader sees and did not cause. The service counts those, and the reader counts the frames its own ring skipped when it
//! was late, so the claim is checked against the counts: every gap the reader sees is a frame
//! the service lost or the ring skipped, and a reader that kept up was skipped nothing. So with
//! no loss upstream and a reader that kept up, there is no gap.

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

/// Frames the service did not have for its readers, since it started: identities its camera's
/// stream skipped, and frames its ring had no buffer for.
fn lost_upstream(service: &Service) -> u64 {
    let lines = list(service);
    let line = lines
        .iter()
        .filter_map(|l| CameraLine::parse(l))
        .find(|c| c.id == CAMERA)
        .expect("the camera is listed");
    line.number("skipped").expect("skipped in the status")
        + line
            .number("publish_dropped")
            .expect("publish_dropped in the status")
}

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
    let lemnos = dir.path().join("lemnos.sock");
    let _daemon = daemon(&lemnos);
    let service = start_service(dir.path(), &lemnos);
    wait_publishing(&service);
    let socket = socket_of(dir.path());
    let _ = parse_fps_milli("120.00").expect("the rate parses");
    // One frame each 8 333 333 ns at 120 fps; the keep window is read from the status.
    let period_ns = 1e9 / 120.0;
    let keep = keep_of(&service);

    for isp in [None, Some("NV12")] {
        let arg = isp.map(|c| c.parse::<SoftIspArg>().expect("arg"));
        // Read before the reader connects: every frame it will see is published after this.
        let lost_before = lost_upstream(&service);
        let mut session =
            Session::connect(&socket, arg.clone().map(IspRunner::new)).expect("connect");
        // Warm-up: the first frames make the ISP's stage and pool, and fill the reader's tables.
        session.run(None, |s| s.delivered >= 200).expect("warm-up");
        let mut stop = None;
        let made = counted(|| {
            stop = Some(
                session
                    .run(None, |s| s.delivered >= 400)
                    .expect("steady state"),
            );
        });
        assert_eq!(session.stats.delivered, 400);
        println!(
            "steady state (isp {isp:?}): 200 frames, {made} allocations on the read loop, {} gaps, keep {keep}",
            session.stats.gaps
        );
        assert_eq!(
            made, 0,
            "the read loop made {made} allocations for 200 frames"
        );
        // Read after the last frame was taken: every loss that could show as a gap is in it.
        let lost = lost_upstream(&service) - lost_before;
        let skipped = session.reader_stats().skipped;
        println!(
            "isp {isp:?}: the reader saw {} gaps; the service lost {lost} frames upstream and the reader's ring skipped {skipped}",
            session.stats.gaps
        );
        // Every gap is accounted for: a frame the service did not have, or one the ring skipped
        // because this reader was late for it. One gap is at least one such frame.
        assert!(
            session.stats.gaps <= lost + skipped,
            "the reader saw {} gaps; the service lost {lost} frames upstream and the reader's \
             ring skipped {skipped}: a gap with no frame behind it",
            session.stats.gaps
        );
        // The claim: the ring skips nothing for a reader that keeps up, with or without the ISP.
        // "Keeps up" is the age of the frame it wanted when it came back for it, not the latency
        // of the frame it got (after a skip that one is younger): the time since the previous
        // capture below `keep` periods (see tests/read.rs for the window).
        let bound_ns = keep as f64 * period_ns;
        let since_previous = session.stats.since_previous_capture_max.map(|ns| ns as f64);
        if since_previous.is_some_and(|m| m >= bound_ns) {
            println!(
                "isp {isp:?}: previous capture to next frame max {since_previous:?} ns is not below {bound_ns} ns: the reader was held up; no skip by the ring not asserted ({} skipped)",
                session.reader_stats().skipped
            );
        } else {
            assert_eq!(
                session.reader_stats().skipped,
                0,
                "the ring skipped no frame for a reader that kept up"
            );
        }
        assert!(stop.is_some());
    }
    service.shutdown();
}
