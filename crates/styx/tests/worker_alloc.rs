//! Allocations per frame of the native capture workers, on a simulated camera (`styx::sim`):
//! the same `NativeCamera`, capture queue, sensor driver and control schedule as on a board,
//! with the frames rendered by a simulated sensor. Counted by thread, after a warm-up:
//!
//! - raw frames (`styx-native-capture`): dequeue, the frame's lease, delivery;
//! - processed frames (`styx-native-softisp` and its `styx-softisp-*` helpers): dequeue, the
//!   software ISP into a pooled output buffer, statistics, the 3A algorithms and their sensor
//!   requests, the lease, delivery;
//!
//! and the consumer (the test's thread) receiving and dropping each frame. The simulated
//! sensor's thread (`styx-sim-sensor`, rendering the scene) is not the product and is left out.
//! What a board adds is the kernel's side of the queue (`VIDIOC_DQBUF`/`QBUF`), which allocates
//! nothing in Styx.
//!
//! `STYX_WORKER_ALLOC_TRACE=1` prints the call stack of each counted allocation.

#![cfg(all(target_os = "linux", feature = "sim"))]
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use styx::prelude::*;
use styx::sim::{SimSource, SimSpec};

struct Counting;

static ARMED: AtomicBool = AtomicBool::new(false);
static TRACE: AtomicBool = AtomicBool::new(false);
static COUNT: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Whether this thread's allocations count: 0 not known yet, 1 yes, 2 no.
    static COUNTED: Cell<u8> = const { Cell::new(0) };
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

/// The capture workers, by name (no allocation: a fixed buffer). The kernel keeps 15 bytes of
/// a thread's name: the names are compared as far as that.
fn is_worker() -> bool {
    let mut name = [0u8; 32];
    // SAFETY: writes at most `name.len()` bytes (NUL-terminated) for the calling thread.
    unsafe { libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), name.len()) };
    [
        &b"styx-native-capture"[..],
        b"styx-native-softisp",
        b"styx-softisp-",
    ]
    .iter()
    .any(|p| name.starts_with(&p[..p.len().min(15)]))
}

fn note() {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let counted = COUNTED
        .try_with(|c| {
            if c.get() == 0 {
                c.set(if is_worker() { 1 } else { 2 });
            }
            c.get() == 1
        })
        .unwrap_or(false);
    if !counted {
        return;
    }
    COUNT.fetch_add(1, Ordering::Relaxed);
    if TRACE.load(Ordering::Relaxed) && !INSIDE.with(|i| i.replace(true)) {
        eprintln!(
            "allocation:\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        INSIDE.with(|i| i.set(false));
    }
}

// SAFETY: forwards to the system allocator unchanged; the bookkeeping touches atomics,
// const-initialised thread-locals and a stack buffer, none of which allocate (the trace, on
// request, allocates with the count's own thread-local guard set).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const WARM: usize = 90;
const FRAMES: usize = 120;

/// Allocations per frame of a capture of `request` from a simulated OV9782, once warm, and the
/// format of its frames.
fn per_frame(request: FrameRequest) -> (f64, FourCc) {
    TRACE.store(
        std::env::var_os("STYX_WORKER_ALLOC_TRACE").is_some_and(|v| v != "0"),
        Ordering::Relaxed,
    );
    let mut spec = SimSpec::new("ov9782");
    spec.source = SimSource::Chart { light: 1.0 };
    let device = styx::sim::device(spec).unwrap();
    let plan = styx::planner::plan_frames(&device, &request).unwrap();
    let mut frames = plan.start().unwrap();
    let mut format = None;
    COUNTED.with(|c| c.set(1));
    let mut next = || match frames.next_frame(Duration::from_secs(3)) {
        RecvOutcome::Data(f) => format = Some(f.meta().format.code),
        RecvOutcome::Empty => panic!("no frame within 3 s"),
        RecvOutcome::Closed => panic!("the capture ended"),
    };
    for _ in 0..WARM {
        next();
    }
    COUNT.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
    for _ in 0..FRAMES {
        next();
    }
    ARMED.store(false, Ordering::SeqCst);
    let n = COUNT.load(Ordering::SeqCst);
    (n as f64 / FRAMES as f64, format.expect("frames"))
}

#[test]
#[allow(clippy::print_stdout)]
fn the_capture_workers_allocate_nothing_per_frame() {
    let mut report = Vec::new();
    let (raw, code) = per_frame(Frames::any().size(640, 400).fps(30));
    report.push(format!("raw {code:?}: {raw:.2}"));
    let (processed, code) = per_frame(Frames::nv12().size(640, 400).fps(30));
    report.push(format!("software ISP {code:?}: {processed:.2}"));
    println!("allocations per frame: {}", report.join(", "));
    assert_eq!((raw, processed), (0.0, 0.0), "{report:?}");
}
