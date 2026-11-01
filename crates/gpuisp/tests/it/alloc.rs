//! A GPU ISP frame's Rust allocations, counted once the stream has warmed up (the driver's own
//! are not the Rust allocator's). Skipped without a Vulkan device.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use crate::common::{mosaic, pack};
use styx_gpuisp::{GpuIsp, Input};
use styx_softisp::*;

struct Counting;

thread_local! {
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn a_frame_from_memory_allocates_once_at_most_when_warm() {
    let (w, h) = (128usize, 96usize);
    let m = mosaic(w, h, CfaPattern::Rggb, |x, y| {
        [
            (x % 1024) as u16,
            ((x + y) % 1024) as u16,
            (y % 1024) as u16,
        ]
    });
    let (raw, stride) = pack(&m, w, h, RawPacking::Csi2Raw10);
    let format = RawFormat::new(w as u32, h as u32, CfaPattern::Rggb, RawPacking::Csi2Raw10);
    let params = IspParams {
        black_level: Some(BlackLevel::uniform(64)),
        tone: Some(ToneCurve::Srgb),
        arithmetic: Arithmetic::Int,
        ..IspParams::default()
    };
    let Ok(mut isp) = GpuIsp::new(format, params) else {
        eprintln!("no Vulkan device: skipped");
        return;
    };
    let mut out = vec![0u8; w * h * 3];
    let mut run = || {
        let buffers = OutputBuffers::Rgb24 {
            data: &mut out,
            stride: w * 3,
        };
        isp.process_input(Input::Bytes(&raw), stride, Scale::Full, buffers)
            .unwrap();
    };
    for _ in 0..20 {
        run();
    }
    const FRAMES: u64 = 100;
    let before = COUNT.with(Cell::get);
    for _ in 0..FRAMES {
        run();
    }
    let per_frame = (COUNT.with(Cell::get) - before) as f64 / FRAMES as f64;
    println!("GPU ISP from memory, allocations per frame: {per_frame:.2}");
    assert!(
        per_frame <= 1.0,
        "{per_frame} allocations per GPU ISP frame"
    );
}
