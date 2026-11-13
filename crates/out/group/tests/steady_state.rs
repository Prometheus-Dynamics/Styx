//! The brief's proof: three cameras through a real frame pool, 10,000 ticks, every push, poll,
//! pop and release in a steady state, with a counting allocator: no allocation after warm-up.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_group::{DropReason, GroupConfig, GroupPolicy, Grouper};

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

const TICKS: u64 = 10_000;
const WARM_UP: u64 = 16;
const PERIOD: u64 = 33_333_333;
const CAMERAS: usize = 3;

/// Deterministic jitter in [0, span): xorshift64*.
fn jitter(state: &mut u64, span: u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d) % span
}

#[test]
fn three_cameras_group_and_release_without_allocating_after_warm_up() {
    let (width, height) = (64u32, 48u32);
    let layouts = packed_layouts(FourCc::NV12, width, height, Some(16), None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::NV12, Resolution::new(width, height).unwrap());
    // Enough buffers for every frame the grouper can hold (3 x (3 + 2) x 3 members) and one more.
    let pool: FramePool<()> =
        FramePool::host(32, BufferSize::new(layouts.span().unwrap(), 256)).unwrap();

    let mut g: Grouper<FrameLease<()>, CAMERAS> =
        Grouper::new(GroupConfig::new(PERIOD / 2).policy(GroupPolicy::Strict));
    for _ in 0..CAMERAS {
        g.add_camera().unwrap();
    }

    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next_id = 0u64;
    let mut groups = 0u64;
    let mut members = 0u64;
    let mut checksum = 0u64;
    let mut before = 0u64;

    for tick in 0..TICKS {
        if tick == WARM_UP {
            before = allocations();
        }
        let base = tick * PERIOD;
        for cam in 0..CAMERAS {
            // Each camera's timestamp: its own phase plus up to 0.5 ms of jitter.
            let ts = base + cam as u64 * 2_000_000 + jitter(&mut rng, 500_000);
            let mut frame = pool
                .acquire(FrameEntry::new(next_id, ts, format, layouts))
                .expect("a free buffer");
            next_id += 1;
            {
                let mut planes = frame.write().unwrap();
                planes.plane(0).unwrap().data[0] = cam as u8;
            }
            let arrived = ts + 5_000_000;
            let _ = g.push_frame(cam, frame, arrived);
        }
        g.poll(base + 5_000_000 + 2_000_000 * CAMERAS as u64);
        while let Some(group) = g.pop() {
            groups += 1;
            members += group.len() as u64;
            for cam in 0..CAMERAS {
                let frame = group.get(cam).unwrap();
                let read = frame.read().unwrap();
                checksum = checksum.wrapping_add(u64::from(read.plane(0).unwrap().data[0]));
            }
            // Dropping the group releases its three buffers back to the pool.
            drop(group);
        }
    }
    let after = allocations();
    // Drain what is still held, then the grouper must be empty.
    g.poll(u64::MAX / 2);
    while g.pop().is_some() {}
    assert_eq!(g.held(), 0);

    let stats = g.stats();
    println!(
        "ticks {TICKS} frames {} groups {groups} members {members} allocations after warm-up {} checksum {checksum} stale {} unmatched {}",
        stats.frames_received(),
        after - before,
        stats.dropped(DropReason::Stale),
        stats.dropped(DropReason::Unmatched),
    );
    assert_eq!(after - before, 0, "steady state allocated");
    assert_eq!(groups, TICKS, "one group per tick");
    assert_eq!(members, TICKS * CAMERAS as u64);
    assert_eq!(stats.drops_total(), 0);
}
