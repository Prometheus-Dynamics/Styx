//! The brief's proof for the controller: replaying a recording through `process` allocates
//! nothing per frame once the loop has warmed up, with a counting allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs::File;
use std::io::BufReader;

use styx_aaa::Tuning;
use styx_aaa::replay::Recording;
use styx_controller::Controller;

const RECORDING: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../aaa/tests/data/replay-sim.jsonl"
);
const WARM_UP: usize = 16;

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
fn process_allocates_nothing_per_frame_after_warm_up() {
    let file = File::open(RECORDING).expect("the aaa recording");
    let rec = Recording::read(BufReader::new(file)).expect("a recording");
    assert!(rec.records.len() > WARM_UP, "the recording is long enough");

    let mut c = Controller::new(&Tuning::default(), rec.config.clone()).unwrap();
    c.start().unwrap();
    for r in &rec.records[..WARM_UP] {
        c.process(&r.stats, &r.meta).unwrap();
    }

    // The counter sees allocations at all (a probe that must count one).
    let probe = allocations();
    std::hint::black_box(vec![1u8; 64]);
    assert!(allocations() > probe, "the counting allocator counts");

    let before = allocations();
    let mut frames = 0u64;
    for r in &rec.records[WARM_UP..] {
        let step = c.process(&r.stats, &r.meta).unwrap();
        std::hint::black_box(step.isp);
        frames += 1;
    }
    let allocs = allocations() - before;
    println!("{frames} frames after warm-up: {allocs} allocations");
    assert_eq!(allocs, 0, "allocations in {frames} steady-state frames");
}
