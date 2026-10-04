//! Styx's platform-neutral crates without `std`.
//!
//! `scripts/check-nostd.sh` builds this library for bare-metal targets (Cortex-M33
//! `thumbv8m.main-none-eabihf`, RISC-V `riscv32imac-unknown-none-elf`) and WebAssembly, and runs
//! its logic as a host test: its dependencies are built without their `std` features there too
//! (`cargo test -p styx-nostd-smoke`), so the float maths go through `libm`.
//!
//! * [`run_3a`]: AE / AWB (`styx-algo`) from a TOML tuning over the simulator's synthetic
//!   sensor, scene and statistics, until AE locks.
//! * [`run_isp`]: the software ISP (`styx-softisp`) over a tiny synthetic RAW10 frame with the
//!   3A's colour gains, statistics included.
//! * [`sensor_timing`]: a sensor description (`styx-sensor`, the built-in OV9782) parsed from
//!   TOML, and its frame length for a rate; [`compiled_sensor_timing`] the same from the
//!   description compiled at build time (`build.rs`, no TOML parsing on the target).
//! * [`dng_round_trip`]: a DNG (`styx-dng`) written and read back in memory.
//! * [`pisp_stats`]: PiSP front end statistics decoded (`styx-pisp`).
//!
//! An application on a microcontroller provides a global allocator and a panic handler; this
//! library needs nothing else.

#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use styx_algo::sim::{Scene, SensorModel, Simulation};
use styx_algo::{CameraConfig, Pipeline, Tuning};
use styx_core::format::FourCc;
use styx_softisp::{
    CfaPattern, IspParams, OutputBuffers, RawFormat, RawPacking, Scale, SoftIsp, StatsConfig,
    WhiteBalance,
};

/// A tuning in Styx's TOML: styx-algo's simulation tuning (AE, and Bayesian AWB with the
/// simulated sensor's colour temperature curve; Raspberry Pi imx219 data, see the file).
pub const TUNING: &str = include_str!("../../../crates/algo/tests/data/sim.toml");

/// What [`run_3a`] ends with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThreeA {
    /// Frames until AE locked (`None`: it did not).
    pub ae_locked_at: Option<usize>,
    /// The last frame's exposure in microseconds.
    pub exposure_us: f64,
    /// The last frame's analogue gain.
    pub analogue_gain: f64,
    /// AWB's colour gains (R, G, B).
    pub colour_gains: [f64; 3],
    /// AWB's colour temperature (K).
    pub colour_temperature: f64,
}

/// AE / AWB over `frames` simulated frames of a constant scene (`lux`, colour temperature `ct`).
pub fn run_3a(frames: u64, lux: f64, ct: f64) -> ThreeA {
    let tuning = Tuning::from_toml_str(TUNING).expect("tuning parses");
    let mut pipeline = Pipeline::from_tuning(&tuning).expect("pipeline from tuning");
    let config = CameraConfig {
        exposure_limits: (Duration::from_micros(20), Duration::from_millis(100)),
        exposure_margin: Duration::from_micros(200),
        frame_duration_limits: (Duration::from_nanos(33_333_333), Duration::from_millis(100)),
        analogue_gain_limits: (1.0, 16.0),
        ..CameraConfig::default()
    };
    let start = pipeline
        .prepare(&config)
        .expect("pipeline prepares")
        .clone();
    let mut sim = Simulation::new(SensorModel::default(), Scene::constant(lux, ct), &config);
    if let Some(s) = start.sensor {
        sim.start_with(
            s.exposure.as_secs_f64(),
            s.analogue_gain,
            s.frame_duration.as_secs_f64(),
        );
    }
    let out = sim.run(&mut pipeline, frames);
    let last = out.last().expect("at least one frame");
    ThreeA {
        ae_locked_at: out.iter().position(|f| f.params.ae.locked),
        exposure_us: last.meta.exposure.as_secs_f64() * 1e6,
        analogue_gain: last.meta.analogue_gain,
        colour_gains: last.params.colour_gains,
        colour_temperature: last.params.colour_temperature,
    }
}

/// What [`run_isp`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct IspRun {
    /// RGB24 pixels.
    pub rgb: Vec<u8>,
    /// Output width and height.
    pub size: (u32, u32),
    /// The statistics' luma histogram total (samples counted).
    pub histogram_samples: u64,
    /// Mean R, G, B of the statistics' zones (white-balanced, 12-bit scale).
    pub zone_means: [f64; 3],
}

/// A `width` x `height` BGGR RAW10 frame (16-bit little-endian words): a horizontal ramp, the
/// red and blue sites scaled by `1 / gains` as a sensor under a coloured light would see grey.
pub fn synthetic_raw(width: u32, height: u32, gains: [f64; 3]) -> Vec<u8> {
    let mut raw = vec![0u8; width as usize * height as usize * 2];
    for y in 0..height {
        for x in 0..width {
            let level = 64.0 + 800.0 * f64::from(x) / f64::from(width - 1);
            let channel = match (y % 2, x % 2) {
                (0, 0) => 2, // B
                (1, 1) => 0, // R
                _ => 1,
            };
            let v = (level / gains[channel]).clamp(0.0, 1023.0) as u16;
            let i = (y * width + x) as usize * 2;
            raw[i..i + 2].copy_from_slice(&v.to_le_bytes());
        }
    }
    raw
}

/// The software ISP over [`synthetic_raw`] with white balance `gains`.
pub fn run_isp(width: u32, height: u32, gains: [f64; 3]) -> IspRun {
    let format = RawFormat::new(
        width,
        height,
        CfaPattern::Bggr,
        RawPacking::U16Le { bits: 10 },
    );
    let params = IspParams {
        white_balance: Some(WhiteBalance {
            r: gains[0] as f32,
            g: gains[1] as f32,
            b: gains[2] as f32,
        }),
        stats: Some(StatsConfig {
            zones_x: 2,
            zones_y: 2,
            ..StatsConfig::default()
        }),
        ..IspParams::default()
    };
    let mut isp = SoftIsp::new(format, params).expect("ISP parameters");
    let raw = synthetic_raw(width, height, gains);
    let (w, h) = isp.output_size(Scale::Full);
    let mut rgb = vec![0u8; w as usize * h as usize * 3];
    let stats = isp
        .process(
            &raw,
            width as usize * 2,
            Scale::Full,
            OutputBuffers::Rgb24 {
                data: &mut rgb,
                stride: w as usize * 3,
            },
        )
        .expect("frame processes")
        .expect("statistics asked for");
    let mut sums = [0.0f64; 3];
    let mut counted = 0.0f64;
    for z in &stats.zones {
        sums[0] += z.r_sum as f64;
        sums[1] += z.g_sum as f64;
        sums[2] += z.b_sum as f64;
        counted += f64::from(z.count);
    }
    let counted = counted.max(1.0);
    IspRun {
        rgb,
        size: (w, h),
        histogram_samples: stats.histogram.iter().map(|&n| u64::from(n)).sum(),
        zone_means: sums.map(|s| s / counted),
    }
}

/// The built-in OV9782's frame length (lines) at `fps` in its 1280x800 RAW10 mode, and the rate
/// that length gives.
pub fn sensor_timing(fps: f64) -> (u32, f64) {
    let (_, toml) = styx_sensor::BUILTIN_DESCRIPTIONS
        .iter()
        .find(|(name, _)| *name == "ov9782")
        .expect("built-in OV9782");
    let desc = styx_sensor::SensorDescription::from_toml_str(toml, "builtin:ov9782")
        .expect("description parses");
    let timing = desc.timing("1280x800", "raw10").expect("mode and format");
    let fl = timing.frame_length_for_fps(fps);
    (fl.lines, fl.fps)
}

/// [`sensor_timing`] from the description compiled by `build.rs` (`styx_sensor::build`): no
/// TOML parser runs here.
pub fn compiled_sensor_timing(fps: f64) -> (u32, f64) {
    let desc =
        styx_sensor::SensorDescription::from_postcard(styx_sensor::include_description!("ov9782"))
            .expect("compiled description");
    let timing = desc.timing("1280x800", "raw10").expect("mode and format");
    let fl = timing.frame_length_for_fps(fps);
    (fl.lines, fl.fps)
}

/// Writes `samples` (`width` x `height`, 10-bit RGGB) as a DNG in memory and reads it back:
/// the samples read.
pub fn dng_round_trip(width: u32, height: u32, samples: Vec<u16>) -> Vec<u16> {
    let image = styx_dng::RawImage::new(
        width,
        height,
        styx_dng::SampleLayout::Cfa(styx_dng::CfaPattern::Rggb),
        10,
        samples,
    )
    .expect("raw image");
    let meta = styx_dng::DngMetadata {
        black_level: [64.0; 4],
        capture_time: Some(Duration::from_secs(1_791_000_000)),
        ..styx_dng::DngMetadata::default()
    };
    let bytes = styx_dng::write_dng(&image, &meta).expect("DNG writes");
    styx_dng::read_dng(&bytes).expect("DNG reads").samples
}

/// PiSP front end statistics from an all-zero buffer: the histogram's sample count.
pub fn pisp_stats() -> u64 {
    let bytes = vec![0u8; core::mem::size_of::<styx_pisp::uapi::RawStatistics>()];
    styx_pisp::stats::Statistics::parse(&bytes)
        .expect("statistics buffer")
        .histogram_count()
}

/// The CFA pattern and packing of a `styx-core` FourCC (formats are `no_std` too).
pub fn bayer_format(code: FourCc) -> Option<(CfaPattern, RawPacking)> {
    styx_softisp::bayer_fourcc(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gains that make grey grey under `ct` for the simulated sensor.
    fn truth(ct: f64) -> [f64; 3] {
        let (r, _, b) = SensorModel::default().illuminant(ct);
        [1.0 / r, 1.0, 1.0 / b]
    }

    #[test]
    fn ae_locks_and_awb_finds_the_light() {
        for ct in [3000.0, 6000.0] {
            let run = run_3a(60, 400.0, ct);
            assert!(run.ae_locked_at.is_some_and(|f| f < 30), "{run:?}");
            assert!(run.exposure_us > 0.0 && run.analogue_gain >= 1.0, "{run:?}");
            let t = truth(ct);
            for c in [0, 2] {
                assert!(
                    (run.colour_gains[c] / t[c] - 1.0).abs() < 0.05,
                    "{run:?} {t:?}"
                );
            }
            assert!((run.colour_temperature - ct).abs() < 300.0, "{run:?}");
        }
    }

    #[test]
    fn isp_renders_the_ramp_grey() {
        let gains = [1.8, 1.0, 1.5];
        let run = run_isp(32, 16, gains);
        assert_eq!(run.size, (32, 16));
        assert_eq!(run.rgb.len(), 32 * 16 * 3);
        // White-balanced: the zones' R, G, B means agree.
        let [r, g, b] = run.zone_means;
        assert!(
            (r / g - 1.0).abs() < 0.05 && (b / g - 1.0).abs() < 0.05,
            "{r} {g} {b}"
        );
        // The ramp: brighter to the right, grey in the middle of the frame.
        let px = |x: usize, y: usize| {
            let i = (y * 32 + x) * 3;
            [run.rgb[i], run.rgb[i + 1], run.rgb[i + 2]]
        };
        let (left, right) = (px(4, 8), px(28, 8));
        assert!(right[1] > left[1] + 50, "{left:?} {right:?}");
        let mid = px(16, 8);
        assert!(
            mid[0].abs_diff(mid[1]) <= 6 && mid[2].abs_diff(mid[1]) <= 6,
            "{mid:?}"
        );
        assert!(run.histogram_samples > 0);
    }

    #[test]
    fn sensor_dng_and_pisp_work_without_std() {
        let (lines, fps) = sensor_timing(30.0);
        assert!(lines > 800 && (fps - 30.0).abs() < 0.05, "{lines} {fps}");
        assert_eq!(compiled_sensor_timing(30.0), (lines, fps));
        let samples: Vec<u16> = (0..16 * 8).map(|i| 64 + (i * 7) % 900).collect();
        assert_eq!(dng_round_trip(16, 8, samples.clone()), samples);
        assert_eq!(pisp_stats(), 0);
        assert_eq!(
            bayer_format(FourCc::new(*b"pBAA")),
            Some((CfaPattern::Bggr, RawPacking::Csi2Raw10))
        );
    }
}
