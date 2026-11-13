//! Steady state: after warm-up, the raw converters allocate nothing per frame (counted by a
//! global allocator on this thread). MJPEG: the count for zune-jpeg is measured and printed; it
//! allocates inside each decode (its decoder state is built per frame), so it is reported, not
//! asserted to be zero.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::*;
use styx_core::{Converter, Decoder};
use styx_decode::{MjpegDecoder, RawConverter, Source, Target};
use styx_format::{ColorSpace, FourCc};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards every call to `System` unchanged; it only counts this thread's allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: the caller's contract for `alloc` is passed on unchanged.
        unsafe { System.alloc(layout) }
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

const FRAMES: u64 = 2_000;
const WARM_UP: u64 = 16;

#[test]
fn raw_converters_allocate_nothing_per_frame() {
    let (w, h) = (64usize, 48usize);
    let (ys, uvs) = (w, w);
    let y = noise(ys * h, 1);
    let uv = noise(uvs * h / 2, 2);
    let in_pool = pool(4, ys * h + uvs * h / 2);
    let input = frame(
        &in_pool,
        FourCc::NV12,
        w as u32,
        h as u32,
        ColorSpace::Bt709,
        &[(0, ys, &y), (ys * h, uvs, &uv)],
    );
    let out_pool = pool(4, w * h * 3);
    let mut conv = RawConverter::new(Source::Nv12, Target::Rgb24).unwrap();
    let mut out = empty_output(&out_pool, FourCc::RG24, w as u32, h as u32);

    for _ in 0..WARM_UP {
        conv.convert(&input, &mut out).unwrap();
    }
    let before = allocations();
    for _ in 0..FRAMES {
        conv.convert(&input, &mut out).unwrap();
    }
    let per_frame = (allocations() - before) as f64 / FRAMES as f64;
    eprintln!("nv12 to rgb24: {per_frame} allocations per frame after warm-up");
    assert_eq!(per_frame, 0.0);
}

#[test]
fn zune_mjpeg_allocations_per_frame_are_reported() {
    let bytes = include_bytes!("data/gradient_61x37_q95_420.jpg");
    let in_pool = pool(2, bytes.len());
    let input = frame(
        &in_pool,
        FourCc::MJPG,
        61,
        37,
        ColorSpace::Srgb,
        &[(0, 0, bytes)],
    );
    let out_pool = pool(2, 61 * 37 * 3);
    let mut dec = MjpegDecoder::zune(FourCc::MJPG);
    let mut out = empty_output(&out_pool, FourCc::RG24, 61, 37);
    for _ in 0..WARM_UP {
        dec.decode(&input, &mut out).unwrap();
    }
    let before = allocations();
    for _ in 0..200u64 {
        dec.decode(&input, &mut out).unwrap();
    }
    let per_frame = (allocations() - before) as f64 / 200.0;
    eprintln!("zune-jpeg 61x37: {per_frame} allocations per decoded frame (inside the library)");
}

#[test]
fn turbojpeg_allocations_per_frame_are_reported() {
    let mut dec = match MjpegDecoder::turbojpeg(FourCc::MJPG) {
        Ok(d) => d,
        Err(styx_core::CodecError::Unavailable(why)) => {
            eprintln!("SKIPPED turbojpeg allocation count: {why}");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let bytes = include_bytes!("data/gradient_61x37_q95_420.jpg");
    let in_pool = pool(2, bytes.len());
    let input = frame(
        &in_pool,
        FourCc::MJPG,
        61,
        37,
        ColorSpace::Srgb,
        &[(0, 0, bytes)],
    );
    let out_pool = pool(2, 61 * 37 * 3);
    let mut out = empty_output(&out_pool, FourCc::RG24, 61, 37);
    for _ in 0..WARM_UP {
        dec.decode(&input, &mut out).unwrap();
    }
    let before = allocations();
    for _ in 0..200u64 {
        dec.decode(&input, &mut out).unwrap();
    }
    let per_frame = (allocations() - before) as f64 / 200.0;
    eprintln!(
        "libturbojpeg 61x37: {per_frame} Rust allocator calls per decoded frame; libturbojpeg uses C malloc, which this counter does not see"
    );
}
