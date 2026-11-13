//! The recorded raw frame and the parameters its goldens were made with, shared by the tests.
//!
//! The goldens in `tests/data` are dev's software ISP (`styx-soft`, the `golden_gen`
//! example run against a scratch copy of `origin/dev`) on `bggr10p_64x48.bin`: a 64x48 BGGR
//! RAW10 frame, CSI-2 packed, 80 bytes a row. `<case>.out` holds the output planes in order, and
//! `<case>.stats` the statistics dev's `IspStats` gave (zones, histogram, gains as f32 bits).

#![allow(dead_code)]

use std::path::PathBuf;

use styx_aaa::{LensShading, Pwl};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferSize, FrameEntry, FrameLease, FramePool, packed_layouts};
use styx_settings::IspSettings;
use styx_soft::params::{Arithmetic, Demosaic, StatsConfig, YuvMatrix};
use styx_soft::{CfaPattern, Options, RawFormat, RawPacking};

pub const W: u32 = 64;
pub const H: u32 = 48;
pub const RAW: FourCc = FourCc::BGGR10P;
pub const RAW_STRIDE: usize = 80;

pub fn raw_format() -> RawFormat {
    RawFormat::new(W, H, CfaPattern::Bggr, RawPacking::Csi2Raw10)
}

pub fn data(name: &str) -> Vec<u8> {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "data", name]
        .iter()
        .collect();
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The radial lens shading grid of dev's `LensShading::radial`, computed the same way in f32.
fn radial(width: u32, height: u32, strength: f32) -> Vec<f64> {
    (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                let fx = 2.0 * x as f32 / (width.max(2) - 1) as f32 - 1.0;
                let fy = 2.0 * y as f32 / (height.max(2) - 1) as f32 - 1.0;
                (1.0 + strength * (fx * fx + fy * fy) / 2.0) as f64
            })
        })
        .collect()
}

/// The neutral settings equal to dev's golden parameters (black level 64 of 1023, white balance
/// 1.9 / 1.0 / 1.6, digital gain 1.3, a colour matrix, an optional tone curve and lens shading).
pub fn settings(shaded: bool, tone: bool) -> IspSettings {
    let mut s = IspSettings::neutral(64.0 / 1023.0);
    s.wb = [1.9, 1.0, 1.6];
    s.digital_gain = 1.3;
    s.ccm = [1.6, -0.4, -0.2, -0.3, 1.5, -0.2, -0.1, -0.5, 1.6];
    s.gamma = tone.then(|| {
        Pwl::new(vec![
            (0.0, 0.0),
            (0.05, 0.2),
            (0.25, 0.55),
            (0.6, 0.85),
            (1.0, 1.0),
        ])
        .unwrap()
    });
    s.lens_shading = shaded.then(|| {
        let r = radial(9, 7, 0.7);
        LensShading {
            width: 9,
            height: 7,
            g: r.clone(),
            b: r.clone(),
            r,
            generation: 0,
        }
    });
    s
}

pub fn stats_config() -> StatsConfig {
    StatsConfig {
        zones_x: 8,
        zones_y: 6,
        histogram_bins: 64,
        saturation: 0.95,
        row_step: 1,
        ..StatsConfig::default()
    }
}

pub fn options(demosaic: Demosaic, yuv: YuvMatrix, stats: bool) -> Options {
    Options {
        demosaic,
        yuv,
        arithmetic: Arithmetic::Auto,
        stats: stats.then(stats_config),
    }
}

/// The recorded frame as a lease from a pool of its own.
pub fn raw_lease(pool: &FramePool<()>, id: u64) -> FrameLease<()> {
    let layouts = packed_layouts(RAW, W, H, None, None).unwrap();
    assert_eq!(layouts[0].stride, RAW_STRIDE, "the golden's stride");
    let format = MediaFormat::with_defaults(RAW, Resolution::new(W, H).unwrap());
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

/// A pool with room for one picture of `code` at `width` x `height`, and its layouts.
pub fn picture_pool(code: FourCc, width: u32, height: u32, buffers: usize) -> FramePool<()> {
    let layouts = packed_layouts(code, width, height, None, None).unwrap();
    FramePool::host(buffers, BufferSize::new(layouts.span().unwrap(), 0)).unwrap()
}

/// A picture lease for `code` at `width` x `height`, `id` as its frame number.
pub fn picture(
    pool: &FramePool<()>,
    code: FourCc,
    width: u32,
    height: u32,
    id: u64,
) -> FrameLease<()> {
    let layouts = packed_layouts(code, width, height, None, None).unwrap();
    let format = MediaFormat::with_defaults(code, Resolution::new(width, height).unwrap());
    pool.acquire(FrameEntry::new(id, 1_000 * id, format, layouts))
        .unwrap()
}
