//! The brief's proof: 10,000 frames through a pool, shared, read, written and released in a
//! steady state, with a counting allocator: no allocation and no copy after warm-up.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FramePool, copy_counters, packed_layouts};

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

#[derive(Debug, Default)]
struct Meta {
    exposure_us: u32,
    gain: u32,
}

const FRAMES: u64 = 10_000;
const WARM_UP: u64 = 16;

#[test]
fn ten_thousand_frames_allocate_and_copy_nothing_after_warm_up() {
    let (width, height) = (64u32, 48u32);
    let layouts = packed_layouts(FourCc::NV12, width, height, Some(16), None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::NV12, Resolution::new(width, height).unwrap());
    let pool: FramePool<Meta> = FramePool::host(4, layouts.span().unwrap());

    // Consumers holding frames for a while, released out of order: kept in a fixed ring so the
    // test itself allocates nothing per frame.
    let mut held: [Option<styx_frame::FrameLease<Meta>>; 3] = [None, None, None];
    let mut checksum = 0u64;
    let mut before = 0u64;

    for n in 0..FRAMES {
        if n == WARM_UP {
            before = allocations();
        }
        let entry = FrameEntry::new(n, 1_000_000 * n, format, layouts);
        let mut frame = pool.acquire(entry).expect("a free buffer");
        {
            let meta = frame.meta_mut().unwrap();
            meta.exposure_us = n as u32;
            meta.gain = 2;
        }
        {
            let mut planes = frame.write().unwrap();
            let luma = planes.plane(0).unwrap();
            luma.data[0] = n as u8;
            luma.data[luma.stride] = (n >> 8) as u8;
        }
        let a = frame.share();
        let b = a.share();
        assert!(frame.write().is_none(), "shared frames are read-only");
        {
            let read = b.read().unwrap();
            let luma = read.plane(0).unwrap();
            checksum = checksum.wrapping_add(u64::from(luma.data[0]));
            assert_eq!(a.meta().exposure_us, n as u32);
            assert_eq!(b.id(), n);
            assert_eq!(b.timestamp_ns(), 1_000_000 * n);
        }
        // Release in a different order each frame; one share stays held for a few frames.
        match n % 3 {
            0 => {
                drop(frame);
                drop(a);
            }
            1 => {
                drop(a);
                drop(frame);
            }
            _ => {
                drop(frame);
                drop(b.share());
                drop(a);
            }
        }
        held[(n % 3) as usize] = Some(b);
    }
    let after = allocations();
    drop(held);

    let copies: usize = copy_counters().iter().map(|c| c.copies()).sum();
    let stats = pool.stats();
    println!(
        "frames {FRAMES}, allocations after warm-up {}, copies {copies}, misses {}, checksum {checksum}",
        after - before,
        stats.misses
    );
    assert_eq!(after - before, 0, "allocations after warm-up");
    assert_eq!(copies, 0, "copies");
    assert_eq!(stats.misses, 0);
    assert_eq!(stats.acquired, FRAMES as usize);
    assert_eq!(stats.in_use, 0);
}
