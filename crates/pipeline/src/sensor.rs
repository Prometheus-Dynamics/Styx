//! The sensor mode as the algorithms and the ISPs see it, from a `styx-sensor` description.

use alloc::format;
use alloc::string::ToString;
use core::time::Duration;
#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;

use serde::{Deserialize, Serialize};
use styx_algo::{CameraConfig, ControlDelays};
use styx_sensor::{ColorFilter, SensorDescription};
use styx_softisp::CfaPattern;

use crate::error::{PipelineError, Result};

/// One sensor mode: geometry, colour filter, bit depth, black level and the algorithms'
/// camera configuration (limits and control delays).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SensorInfo {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Colour filter order of the output.
    pub cfa: CfaPattern,
    /// Bits per sample on the bus.
    pub bits: u8,
    /// Black level, normalised (full scale 1.0).
    pub black_level: f64,
    /// What the algorithms are prepared with.
    pub camera: CameraConfig,
    /// Time to read a frame out (image and embedded data lines), from its start.
    #[serde(default)]
    pub readout: Duration,
}

/// Frames from the statistics of frame `F` to the frame start where a request can be written:
/// statistics of `F` are complete at its end and processed during `F + 1`, so writes go out at
/// the start of `F + 2` at the earliest. The latency without timing knowledge; see
/// [`SensorInfo::issue_latency`] for requests written as soon as they are made.
pub const ISSUE_LATENCY: u32 = 2;

fn cfa(c: ColorFilter) -> Result<CfaPattern> {
    Ok(match c {
        ColorFilter::Bggr => CfaPattern::Bggr,
        ColorFilter::Gbrg => CfaPattern::Gbrg,
        ColorFilter::Grbg => CfaPattern::Grbg,
        ColorFilter::Rggb => CfaPattern::Rggb,
        ColorFilter::Mono => {
            return Err(PipelineError::Sensor(
                "monochrome sensors have no colour pipeline".into(),
            ));
        }
    })
}

impl SensorInfo {
    /// Mode `mode` in format `format` (description names, e.g. `1280x800`, `raw10`).
    pub fn from_description(desc: &SensorDescription, mode: &str, format: &str) -> Result<Self> {
        let err = |e: styx_sensor::SensorError| PipelineError::Sensor(e.to_string());
        let m = desc.mode(mode).map_err(err)?;
        let f = desc.format_for(m, format).map_err(err)?;
        let t = desc.timing(mode, format).map_err(err)?;
        let bits = f.bits();
        let ctl = &desc.controls;
        let g = &ctl.analog_gain;
        let gains = (g.gain_for_code(g.min_code), g.gain_for_code(g.max_code));
        let fl_max = t.frame_length_max();
        let exposure_limits = (
            t.exposure_limits(t.frame_length_min()).min,
            t.exposure_limits(fl_max).max,
        );
        let black = desc
            .pixel_array
            .black_level
            .map(|b| f64::from(b.value) / f64::from(1u32 << b.bits));
        let d = ctl.delays;
        let camera = CameraConfig {
            exposure_limits,
            exposure_margin: t.lines_to_duration(f64::from(ctl.exposure.margin)),
            frame_duration_limits: (
                t.frame_duration(t.frame_length_min()),
                t.frame_duration(fl_max),
            ),
            analogue_gain_limits: gains,
            delays: ControlDelays {
                exposure: d.exposure,
                analogue_gain: d.analog_gain,
                frame_duration: d.frame_length,
                issue_latency: ISSUE_LATENCY,
            },
            sensitivity: 1.0,
            black_level: black,
            unsettled_frames: desc.pixel_array.black_level.map_or(0, |b| b.settle_frames),
            ..Default::default()
        };
        let lines = m.size.height + desc.embedded_data.as_ref().map_or(0, |e| e.lines);
        Ok(Self {
            width: m.size.width,
            height: m.size.height,
            cfa: cfa(desc.color_filter(false, false))?,
            bits,
            black_level: black.unwrap_or(0.0),
            camera,
            readout: t.lines_to_duration(f64::from(lines)),
        })
    }

    /// Frames from a frame's statistics to the frame in which requests made from them are
    /// written, when requests are written as soon as they are made (`request_at_now`): the
    /// statistics of `F` are ready `readout + processing` after `F` starts, and a write must be
    /// done `write_margin` before the frame it is made in ends (the sensor latches its
    /// registers at the next frame start). 0 at 30 fps on the OV9782 (written during `F`,
    /// exposure lands on `F + 2`), 1 at 120 fps; at most [`ISSUE_LATENCY`]. Uses the shortest
    /// frame duration of the mode.
    pub fn issue_latency(&self, processing: Duration, write_margin: Duration) -> u32 {
        let fd = self.camera.frame_duration_limits.0.as_secs_f64();
        let ready = (self.readout + processing + write_margin).as_secs_f64();
        ((ready / fd.max(1e-9)).floor() as u32).min(ISSUE_LATENCY)
    }

    /// The same mode with the frame rate held within `min_fps..=max_fps` (the algorithms then
    /// choose exposures within that frame length; equal values fix the rate).
    pub fn with_fps(mut self, min_fps: f64, max_fps: f64) -> Result<Self> {
        if !(min_fps > 0.0 && min_fps <= max_fps && max_fps.is_finite()) {
            return Err(PipelineError::Config(format!(
                "frame rate range {min_fps}..{max_fps}"
            )));
        }
        let (lo, hi) = self.camera.frame_duration_limits;
        let short = Duration::from_secs_f64(1.0 / max_fps).clamp(lo, hi);
        let long = Duration::from_secs_f64(1.0 / min_fps).clamp(lo, hi);
        self.camera.frame_duration_limits = (short, long);
        let max_exposure = long.saturating_sub(self.camera.exposure_margin);
        self.camera.exposure_limits.1 = self
            .camera
            .exposure_limits
            .1
            .min(max_exposure)
            .max(self.camera.exposure_limits.0);
        Ok(self)
    }

    /// Full scale of a sample (`2^bits - 1`).
    pub fn full_scale(&self) -> u32 {
        (1u32 << self.bits) - 1
    }
}

#[cfg(test)]
pub(crate) fn ov9782() -> SensorDescription {
    SensorDescription::from_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sensor/sensors/ov9782.toml"
    ))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ov9782_mode_for_the_algorithms() {
        let s = SensorInfo::from_description(&ov9782(), "1280x800", "raw10").unwrap();
        assert_eq!(
            (s.width, s.height, s.bits, s.cfa),
            (1280, 800, 10, CfaPattern::Bggr)
        );
        assert!((s.black_level - 64.0 / 1024.0).abs() < 1e-12);
        let c = &s.camera;
        assert_eq!(
            (
                c.delays.exposure,
                c.delays.analogue_gain,
                c.delays.frame_duration
            ),
            (2, 2, 1)
        );
        assert_eq!(c.delays.earliest_landing(10), 14);
        assert_eq!(c.analogue_gain_limits, (1.0, 15.9375));
        // 120.63 fps at the shortest frame (911 lines of 9.1 us).
        assert!((c.frame_duration_limits.0.as_secs_f64() * 1e3 - 8.290).abs() < 0.001);
        c.validate().unwrap();
        let at30 = s.clone().with_fps(30.0, 30.0).unwrap();
        let (lo, hi) = at30.camera.frame_duration_limits;
        assert_eq!(lo, hi);
        assert!((hi.as_secs_f64() - 1.0 / 30.0).abs() < 1e-9);
        assert!(at30.camera.exposure_limits.1 < hi);
        assert!(at30.clone().with_fps(0.0, 1.0).is_err());
        // 801 lines of 9.1 us; written in the same frame at 30 and 60 fps, the next at 120.
        assert!((s.readout.as_secs_f64() * 1e3 - 7.29).abs() < 0.01);
        let (p, m) = (Duration::from_millis(2), Duration::from_millis(4));
        assert_eq!(at30.issue_latency(p, m), 0);
        assert_eq!(
            s.clone().with_fps(60.0, 60.0).unwrap().issue_latency(p, m),
            0
        );
        assert_eq!(
            s.clone()
                .with_fps(120.0, 120.0)
                .unwrap()
                .issue_latency(p, m),
            1
        );
        assert_eq!(s.issue_latency(Duration::from_millis(20), m), 2);
    }
}
