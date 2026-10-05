//! Heap high-water marks of the four configurations at QQVGA and QVGA: the same `no_std` code
//! the firmware images run, under a counting allocator, with the frame buffers (receiver
//! buffers and ISP outputs, allocated inside `styx_mcu_footprint::frame_memory`) counted apart
//! from everything else. `cargo test -p styx-mcu-footprint --test heap -- --nocapture` prints
//! the table. On a 64-bit host pointers, `usize`s and `Vec` headers are twice their size on a
//! 32-bit microcontroller, so the "other" column is an upper bound; docs/mcu.md quotes the
//! same test built for `wasm32-wasip1` (32-bit) and run under Node (`wasi-run.mjs`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use styx_mcu_footprint::{FRAME_MEMORY, QQVGA, QVGA, Report};

/// Frame buffers live at once, at most.
const SLOTS: usize = 16;

struct Counting;

static OTHER: AtomicUsize = AtomicUsize::new(0);
static OTHER_PEAK: AtomicUsize = AtomicUsize::new(0);
static FRAMES: AtomicUsize = AtomicUsize::new(0);
static FRAMES_PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Addresses of the live frame buffers (0: free).
static FRAME_PTRS: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];

fn add(cur: &AtomicUsize, peak: &AtomicUsize, n: usize) {
    let now = cur.fetch_add(n, Ordering::Relaxed) + n;
    peak.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: forwards to the system allocator; the bookkeeping allocates nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller's.
        let p = unsafe { System.alloc(layout) };
        if p.is_null() {
            return p;
        }
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        if FRAME_MEMORY.load(Ordering::Relaxed)
            && let Some(slot) = FRAME_PTRS.iter().find(|s| {
                s.compare_exchange(0, p as usize, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            })
        {
            let _ = slot;
            add(&FRAMES, &FRAMES_PEAK, layout.size());
        } else {
            add(&OTHER, &OTHER_PEAK, layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let frame = FRAME_PTRS.iter().any(|s| {
            s.compare_exchange(p as usize, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        });
        if frame {
            FRAMES.fetch_sub(layout.size(), Ordering::Relaxed);
        } else {
            OTHER.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: as the caller's.
        unsafe { System.dealloc(p, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One configuration's heap.
#[derive(Debug)]
struct Heap {
    frames: usize,
    other: usize,
    allocations: usize,
    report: Report,
}

static SERIAL: Mutex<()> = Mutex::new(());

/// A configuration's run: width, height, frames.
type Run = fn(u32, u32, u32) -> Report;

/// Runs `run` and returns its heap high-water marks above what was in use before.
fn measure(run: impl FnOnce() -> Report) -> Heap {
    let base_other = OTHER.load(Ordering::Relaxed);
    let base_frames = FRAMES.load(Ordering::Relaxed);
    OTHER_PEAK.store(base_other, Ordering::Relaxed);
    FRAMES_PEAK.store(base_frames, Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let report = run();
    Heap {
        frames: FRAMES_PEAK.load(Ordering::Relaxed) - base_frames,
        other: OTHER_PEAK.load(Ordering::Relaxed) - base_other,
        allocations: ALLOCATIONS.load(Ordering::Relaxed) - allocations,
        report,
    }
}

#[test]
fn heap_high_water_marks() {
    let _serial = SERIAL.lock().unwrap();
    let configs: [(&str, Run); 5] = [
        ("A", styx_mcu_footprint::a::run),
        ("B", styx_mcu_footprint::b::run),
        ("C", styx_mcu_footprint::c::run),
        ("D", |w, h, n| {
            styx_mcu_footprint::d::run_with(w, h, n, false)
        }),
        ("D+bracket", styx_mcu_footprint::d::run),
    ];
    println!("config     size     frame buffers   other heap   allocations (60 frames)");
    for (name, run) in configs {
        for (w, h) in [QQVGA, QVGA] {
            let heap = measure(|| run(w, h, 60));
            println!(
                "{name:<10} {w}x{h:<4} {:>10} {:>12} {:>10}",
                heap.frames, heap.other, heap.allocations
            );
            let r = heap.report;
            // Every frame reached the consumer; B's ramp landed frame-exactly (all but the
            // first frames, before the first request); AE locked; the bracket took its shots.
            assert_eq!(r.frames, 60, "{name} at {w}x{h}: {r:?}");
            match name {
                "B" => assert!(r.exact >= 55, "{name} at {w}x{h}: {r:?}"),
                "C" | "D" => assert!(r.ae_locked_at.is_some(), "{name} at {w}x{h}: {r:?}"),
                "D+bracket" => assert_eq!(r.shots, 3, "{name} at {w}x{h}: {r:?}"),
                _ => {}
            }
            // The receiver's three RAW10 buffers in 16-bit samples, and D's NV12 output.
            let raw = 3 * w as usize * h as usize * 2;
            let nv12 = w as usize * h as usize * 3 / 2;
            let expected = if name.starts_with('D') {
                raw + nv12
            } else {
                raw
            };
            assert_eq!(heap.frames, expected, "{name} frame buffers at {w}x{h}");
        }
    }
}
