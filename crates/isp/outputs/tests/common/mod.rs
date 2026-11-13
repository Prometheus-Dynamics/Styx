//! The soft ISP's capability and a recorded raw frame, shared by the tests.
//!
//! `data/bggr10p_64x48.bin` is the 64x48 BGGR RAW10 frame `styx-soft`'s goldens are made from
//! (CSI-2 packed, 80 bytes a row). The soft ISP's outputs are full and half size, and its picture
//! kinds are the ones its stage takes (see `styx-soft`'s `stage.rs`).

#![allow(dead_code)]

use std::path::PathBuf;

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_outputs::{IspCapability, OutputCapability, SizeRule};

pub const W: u32 = 64;
pub const H: u32 = 48;
pub const RAW: FourCc = FourCc::BGGR10P;
pub const RAW_STRIDE: usize = 80;

/// The soft ISP: output 0 full size, output 1 half size, both in the same picture kinds.
pub struct Soft;

const SOFT_FORMATS: [FourCc; 5] = [
    FourCc::RGB3,
    FourCc::GREY,
    FourCc::NV12,
    FourCc::I420,
    FourCc::YU12,
];

const SOFT_OUTPUTS: [OutputCapability; 2] = [
    OutputCapability {
        sizes: SizeRule::Divisor(1),
        formats: &SOFT_FORMATS,
    },
    OutputCapability {
        sizes: SizeRule::Divisor(2),
        formats: &SOFT_FORMATS,
    },
];

impl IspCapability for Soft {
    fn name(&self) -> &str {
        "soft"
    }

    fn outputs(&self) -> &[OutputCapability] {
        &SOFT_OUTPUTS
    }
}

pub fn capture() -> Resolution {
    Resolution::new(W, H).unwrap()
}

pub fn res(width: u32, height: u32) -> Resolution {
    Resolution::new(width, height).unwrap()
}

pub fn data(name: &str) -> Vec<u8> {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "data", name]
        .iter()
        .collect();
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The recorded raw frame as a lease from a pool of its own, frame number `id`.
pub fn raw_lease(id: u64) -> FrameLease<()> {
    let layouts = packed_layouts(RAW, W, H, None, None).unwrap();
    assert_eq!(layouts[0].stride, RAW_STRIDE);
    let pool: FramePool<()> =
        FramePool::host(1, BufferSize::new(layouts.span().unwrap(), 0)).unwrap();
    let format = MediaFormat::with_defaults(RAW, capture());
    let mut lease = pool
        .acquire(FrameEntry::new(id, 1_000 * id, format, layouts))
        .unwrap();
    let bytes = data("bggr10p_64x48.bin");
    {
        let mut planes = lease.write().unwrap();
        let mut plane = planes.plane(0).unwrap();
        for row in 0..H as usize {
            plane
                .row_bytes_mut(row, RAW_STRIDE)
                .unwrap()
                .copy_from_slice(&bytes[row * RAW_STRIDE..][..RAW_STRIDE]);
        }
    }
    lease
}
