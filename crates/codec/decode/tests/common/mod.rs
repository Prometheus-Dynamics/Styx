//! Shared test helpers: frames built from bytes, and the reference YUV formula.
#![allow(dead_code)]

use styx_format::{ColorRange, ColorSpace, FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, Layouts, PlaneLayout};

/// A pool of `count` host buffers, each `payload` bytes.
pub fn pool(count: usize, payload: usize) -> FramePool<()> {
    FramePool::host(count, BufferSize::new(payload, 0)).expect("pool")
}

/// A frame of `code` at `w` x `h` whose planes are `planes`, each a `(offset, stride, bytes)`
/// triple: the bytes are copied to the plane's offset and its length is the byte count.
pub fn frame(
    pool: &FramePool<()>,
    code: FourCc,
    w: u32,
    h: u32,
    color: ColorSpace,
    planes: &[(usize, usize, &[u8])],
) -> FrameLease<()> {
    let layouts: Vec<PlaneLayout> = planes
        .iter()
        .map(|&(offset, stride, bytes)| PlaneLayout::new(offset, stride, bytes.len()))
        .collect();
    let layouts = Layouts::new(&layouts).expect("layouts");
    let format = MediaFormat::new(
        code,
        Resolution::new(w, h).unwrap(),
        color,
        ColorRange::Unknown,
    );
    let mut lease = pool
        .acquire(FrameEntry::new(7, 1_000, format, layouts))
        .expect("acquire");
    {
        let mut planes_mut = lease.write().expect("writable");
        for (i, &(_, _, bytes)) in planes.iter().enumerate() {
            planes_mut
                .plane(i)
                .expect("plane")
                .data
                .copy_from_slice(bytes);
        }
    }
    lease
}

/// An empty output lease from `pool`; the converter or decoder sets its entry.
pub fn empty_output(pool: &FramePool<()>, code: FourCc, w: u32, h: u32) -> FrameLease<()> {
    let layouts = styx_frame::packed_layouts(code, w, h, None, None).expect("layouts");
    let format = MediaFormat::new(
        code,
        Resolution::new(w, h).unwrap(),
        ColorSpace::Unknown,
        ColorRange::Unknown,
    );
    pool.acquire(FrameEntry::new(0, 0, format, layouts))
        .expect("acquire")
}

/// The visible bytes of plane 0 of `lease`, `rows` rows of `row_bytes`, packed.
pub fn packed_rows(lease: &FrameLease<()>, row_bytes: usize, rows: usize) -> Vec<u8> {
    let read = lease.read().expect("readable");
    let plane = read.plane(0).expect("plane 0");
    (0..rows)
        .flat_map(|y| plane.row_bytes(y, row_bytes).expect("row").to_vec())
        .collect()
}

/// Deterministic bytes: an xorshift stream from `seed`.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// The reference YUV to RGB formula, written out from the old per-decoder code: `s` times
/// `max(y - off, 0)`, plus the chroma terms, shifted and clamped. Independent of `YuvMatrix`.
#[derive(Clone, Copy)]
pub struct Old {
    pub off: i32,
    pub s: i32,
    pub rv: i32,
    pub gu: i32,
    pub gv: i32,
    pub bu: i32,
}

pub const OLD_BT709: Old = Old {
    off: 16,
    s: 298,
    rv: 459,
    gu: 55,
    gv: 136,
    bu: 541,
};
pub const OLD_BT2020: Old = Old {
    off: 16,
    s: 298,
    rv: 430,
    gu: 48,
    gv: 166,
    bu: 549,
};
pub const OLD_BT601_FULL: Old = Old {
    off: 0,
    s: 256,
    rv: 359,
    gu: 88,
    gv: 183,
    bu: 454,
};
pub const OLD_BT601_LIMITED: Old = Old {
    off: 16,
    s: 298,
    rv: 409,
    gu: 100,
    gv: 208,
    bu: 516,
};

/// The reference matrix for a frame labelled `color`, as the decoders pick it.
pub fn old_for(color: ColorSpace) -> Old {
    match color {
        ColorSpace::Bt709 | ColorSpace::Unknown => OLD_BT709,
        ColorSpace::Bt2020 => OLD_BT2020,
        ColorSpace::Bt601 => OLD_BT601_LIMITED,
        ColorSpace::Srgb => OLD_BT601_FULL,
    }
}

pub fn old_rgb(m: Old, y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = m.s * (i32::from(y) - m.off).max(0);
    let (d, e) = (i32::from(u) - 128, i32::from(v) - 128);
    [
        c + m.rv * e + 128,
        c - m.gu * d - m.gv * e + 128,
        c + m.bu * d + 128,
    ]
    .map(|x| (x >> 8).clamp(0, 255) as u8)
}
