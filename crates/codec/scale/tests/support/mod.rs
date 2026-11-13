#![allow(dead_code)] // each test binary uses a different part of this module
//! Frames for the tests: a pool of packed frames, filled with a pattern, and their visible bytes.

use styx_format::{FourCc, MediaFormat, Resolution, plane_geometry};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};

/// Largest frame the tests make, in bytes (64 x 64 RGBA).
pub const MAX_BYTES: usize = 64 * 64 * 4;

/// A pool of `count` buffers, each big enough for any test frame.
pub fn pool(count: usize) -> FramePool<()> {
    FramePool::host(count, BufferSize::new(MAX_BYTES, 0)).expect("pool")
}

/// A frame of `code` at `width` x `height` from `pool`, its bytes a pattern from `seed`.
pub fn frame(
    pool: &FramePool<()>,
    code: FourCc,
    width: u32,
    height: u32,
    seed: u32,
) -> FrameLease<()> {
    let format = MediaFormat::with_defaults(code, Resolution::new(width, height).expect("size"));
    let layouts = packed_layouts(code, width, height, None, None).expect("layout");
    let mut lease = pool
        .acquire(FrameEntry::new(u64::from(seed), 0, format, layouts))
        .expect("acquire");
    let mut planes = lease.write().expect("writable");
    for (i, byte) in planes.bytes().iter_mut().enumerate() {
        *byte = pattern(i as u32, seed);
    }
    lease
}

/// An empty frame of `code` at `width` x `height` from `pool`.
pub fn blank(pool: &FramePool<()>, code: FourCc, width: u32, height: u32) -> FrameLease<()> {
    frame(pool, code, width, height, 0)
}

/// Writes `data` at the start of a frame's bytes.
pub fn set(lease: &mut FrameLease<()>, data: &[u8]) {
    let mut planes = lease.write().expect("writable");
    planes.bytes()[..data.len()].copy_from_slice(data);
}

fn pattern(i: u32, seed: u32) -> u8 {
    let mixed = i
        .wrapping_mul(2_654_435_761)
        .wrapping_add(seed.wrapping_mul(40503));
    (mixed >> 13) as u8
}

/// The visible bytes of a frame: each plane's rows at their visible width, one plane after another.
pub fn visible(lease: &FrameLease<()>) -> Vec<u8> {
    let format = lease.format();
    let geometry = plane_geometry(
        format.code,
        format.resolution.width.get(),
        format.resolution.height.get(),
    )
    .expect("geometry");
    let read = lease.read().expect("readable");
    let mut out = Vec::new();
    for (index, g) in geometry.iter().enumerate() {
        let plane = read.plane(index).expect("plane");
        for r in 0..g.rows {
            out.extend_from_slice(plane.row_bytes(r, g.row_bytes).expect("row"));
        }
    }
    out
}
