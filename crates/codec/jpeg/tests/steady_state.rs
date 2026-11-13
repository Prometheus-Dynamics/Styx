//! The brief's proof for the encoder: 400 frames through a pool with a counting allocator. No
//! allocation after warm-up: the strips and coefficients are sized once, the bytes go straight
//! into the caller's output frame.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use styx_codec_api::Encoder;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FramePool, Layouts, PlaneLayout, packed_layouts};
use styx_jpeg::StyxJpegEncoder;

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

const FRAMES: u64 = 400;
const WARM_UP: u64 = 16;

#[test]
fn four_hundred_frames_allocate_nothing_after_warm_up() {
    let (w, h) = (128u32, 96u32);
    let inputs = packed_layouts(FourCc::I420, w, h, Some(64), None).unwrap();
    let in_format = MediaFormat::with_defaults(FourCc::I420, Resolution::new(w, h).unwrap());
    let in_pool: FramePool =
        FramePool::host(3, BufferSize::new(inputs.span().unwrap(), 0)).unwrap();

    let cap = (w * h) as usize;
    let out_pool: FramePool = FramePool::host(3, BufferSize::new(cap, 0)).unwrap();
    let out_entry = |n: u64| {
        FrameEntry::new(
            n,
            n,
            MediaFormat::with_defaults(FourCc::MJPG, Resolution::new(w, h).unwrap()),
            Layouts::new(&[PlaneLayout::new(0, cap, cap)]).unwrap(),
        )
    };

    let mut enc = StyxJpegEncoder::new(FourCc::I420, 80).unwrap();
    let mut before = 0u64;
    let mut bytes = 0usize;
    for n in 0..FRAMES {
        if n == WARM_UP {
            before = allocations();
        }
        let mut input = in_pool
            .acquire(FrameEntry::new(n, n, in_format, inputs))
            .expect("a free input buffer");
        {
            let mut planes = input.write().unwrap();
            for i in 0..3 {
                let plane = planes.plane(i).unwrap();
                for (k, b) in plane.data.iter_mut().enumerate() {
                    *b = (k as u64 + n + i as u64) as u8;
                }
            }
        }
        let mut output = out_pool
            .acquire(out_entry(n))
            .expect("a free output buffer");
        enc.encode(&input, &mut output).unwrap();
        bytes += output.entry().layouts[0].len;
    }
    let after = allocations();
    assert!(bytes > 0);
    assert_eq!(after - before, 0, "allocations after warm-up");
}
