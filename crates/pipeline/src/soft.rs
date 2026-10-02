//! The 3A loop with the software ISP: `styx-softisp` processes each raw frame and gathers its
//! statistics in the same pass, the controller turns them into a sensor request and ISP
//! settings, and those settings process the next frame.

use std::time::Instant;

use styx_algo::{Params, Statistics, Tuning};
use styx_softisp::{
    Demosaic, IspParams, OutputBuffers, RawFormat, RawPacking, Scale, SoftIsp, StatsConfig,
    YuvMatrix,
};

use crate::controller::{Controller, SensorValues, Start, Step};
use crate::error::Result;
use crate::isp::IspSettings;
use crate::sensor::SensorInfo;
use crate::stats;

/// One processed frame.
#[derive(Clone, Debug)]
pub struct SoftOutput {
    /// The loop's output for this frame (its statistics are in `stats`).
    pub step: Step,
    /// The settings the frame was processed with.
    pub applied: IspSettings,
    /// The frame's statistics (empty when the loop skipped them, see [`SETTLED_RATE_HZ`]).
    pub stats: Statistics,
    /// Where the frame's processing time went.
    pub timing: SoftTiming,
}

/// Time spent on one frame by each part of [`SoftLoop::process`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SoftTiming {
    /// Preparing the ISP for new settings (lens shading tables, tone curve).
    pub settings: std::time::Duration,
    /// The software ISP: the picture and its statistics.
    pub isp: std::time::Duration,
    /// Converting the statistics for the algorithms.
    pub stats: std::time::Duration,
    /// The algorithms (AE, AWB, lens shading, colour, tone).
    pub algorithms: std::time::Duration,
}

/// The software ISP loop. See the [module documentation](self).
pub struct SoftLoop {
    info: SensorInfo,
    controller: Controller,
    isp: SoftIsp,
    base: IspParams,
    /// The latest algorithm output and the frame it came from.
    latest: Option<(Params, u64)>,
    start: IspSettings,
    applied: Option<IspSettings>,
    /// While settled, statistics and algorithms run every this many frames.
    settled_every: u64,
    /// The frame the algorithms last ran on.
    last_run: Option<u64>,
}

impl std::fmt::Debug for SoftLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftLoop")
            .field("info", &self.info)
            .field("isp", &self.isp)
            .finish_non_exhaustive()
    }
}

/// Lens shading grids within this (relative) of the one the software ISP's tables were built
/// from keep those tables: adaptive lens shading moves its grid a little on most frames, and a
/// rebuild costs 0.3 ms at 1280x800 on a Cortex-A76 (0.25%: under a code at full scale).
pub const LSC_TOLERANCE: f32 = 0.0025;

/// While AE is locked and AWB has converged, the statistics and the algorithms run at about
/// this rate instead of on every frame (as the PiSP path, [`crate::device::PispOptions`]):
/// every second frame at 30 fps. A frame that finds them unsettled goes back to every frame.
pub const SETTLED_RATE_HZ: f64 = 15.0;

/// Software ISP parameters the loop does not change: bilinear demosaic, full-range BT.601 YUV
/// (as the PiSP back end's "jpeg" encoding), statistics on a 16x12 zone grid with a 256-bin
/// histogram on every fourth quad row (65 536 quads of a 1280x800 frame, 340 per zone; every
/// quad row would cost 0.9 ms of the frame on a Cortex-A76, every fourth 0.23 ms).
pub fn base_params() -> IspParams {
    IspParams {
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt601Full,
        stats: Some(StatsConfig {
            zones_x: 16,
            zones_y: 12,
            histogram_bins: 256,
            saturation: 0.95,
            row_step: 4,
        }),
        ..IspParams::default()
    }
}

impl SoftLoop {
    /// A loop for frames of `info`'s mode stored with `packing`, with the algorithms of
    /// `tuning`; `threads` row bands per frame (needs `styx-softisp`'s `rayon` feature).
    pub fn new(
        info: SensorInfo,
        packing: RawPacking,
        tuning: &Tuning,
        threads: usize,
    ) -> Result<Self> {
        let format = RawFormat::new(info.width, info.height, info.cfa, packing);
        let controller = Controller::new(tuning, info.camera.clone())?;
        let start = IspSettings::neutral(info.black_level);
        let base = base_params();
        let mut isp = SoftIsp::new(format, start.softisp(info.bits, &base))?.with_threads(threads);
        isp.set_lens_shading_tolerance(LSC_TOLERANCE);
        Ok(Self {
            info,
            controller,
            isp,
            base,
            latest: None,
            start,
            applied: None,
            settled_every: 1,
            last_run: None,
        }
        .with_settled_rate(Some(SETTLED_RATE_HZ)))
    }

    /// Run the statistics and algorithms at about `rate_hz` while settled (see
    /// [`SETTLED_RATE_HZ`]); `None`: on every frame.
    pub fn with_settled_rate(mut self, rate_hz: Option<f64>) -> Self {
        self.set_settled_rate(rate_hz);
        self
    }

    /// See [`Self::with_settled_rate`].
    pub fn set_settled_rate(&mut self, rate_hz: Option<f64>) {
        let frame = self.info.camera.frame_duration_limits.0.as_secs_f64();
        self.settled_every = match rate_hz.filter(|r| *r > 0.0) {
            Some(r) if frame > 0.0 => (1.0 / frame / r).round().max(1.0) as u64,
            _ => 1,
        };
    }

    /// Replaces the fixed parameters (demosaic, YUV matrix, statistics set-up).
    pub fn set_base_params(&mut self, base: IspParams) {
        self.base = base;
        self.applied = None;
    }

    /// The controller (controls, recording).
    pub fn controller(&mut self) -> &mut Controller {
        &mut self.controller
    }

    /// The sensor mode.
    pub fn info(&self) -> &SensorInfo {
        &self.info
    }

    /// The raw format processed.
    pub fn format(&self) -> RawFormat {
        self.isp.format()
    }

    /// Resets the algorithms; returns the start-up values (apply the sensor request before
    /// streaming).
    pub fn start(&mut self) -> Result<Start> {
        let s = self.controller.start()?;
        self.start = s.isp.clone();
        self.latest = None;
        self.applied = None;
        self.last_run = None;
        Ok(s)
    }

    /// The settings the next frame (described by `sensor`) is processed with.
    pub fn settings_for(&self, sensor: &SensorValues) -> IspSettings {
        match &self.latest {
            Some((p, from)) => self.controller.isp_for(p, *from, sensor),
            None => self.start.clone(),
        }
    }

    /// Processes one raw frame (`stride` bytes per row) produced with `sensor`'s values into
    /// `out` at `scale`, then runs the algorithms on its statistics.
    pub fn process(
        &mut self,
        raw: &[u8],
        stride: usize,
        sensor: &SensorValues,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<SoftOutput> {
        let t0 = Instant::now();
        let settings = self.settings_for(sensor);
        if self.applied.as_ref() != Some(&settings) {
            self.isp
                .set_params(settings.softisp(self.info.bits, &self.base))?;
            self.applied = Some(settings.clone());
        }
        // Settled: statistics and algorithms only every few frames.
        let settled = self.controller.controls().ae_enable
            && self
                .latest
                .as_ref()
                .is_some_and(|(p, _)| p.ae.locked && p.awb.converged);
        let run = !settled
            || self
                .last_run
                .is_none_or(|r| sensor.frame >= r + self.settled_every);
        self.isp.set_statistics(run);
        let t1 = Instant::now();
        let raw_stats = self.isp.process(raw, stride, scale, out)?;
        let t2 = Instant::now();
        let stats = raw_stats
            .map(|s| {
                stats::from_softisp(&s, self.info.black_level, settings.lens_shading.is_some())
            })
            .unwrap_or_default();
        let t3 = Instant::now();
        let step = match (&self.latest, run) {
            (Some((params, _)), false) => Step {
                frame: sensor.frame,
                sensor: None,
                isp: settings.clone(),
                params: params.clone(),
            },
            _ => {
                let step = self.controller.process(&stats, sensor)?;
                self.latest = Some((step.params.clone(), step.frame));
                self.last_run = Some(sensor.frame);
                step
            }
        };
        let t4 = Instant::now();
        Ok(SoftOutput {
            step,
            applied: settings,
            stats,
            timing: SoftTiming {
                settings: t1 - t0,
                isp: t2 - t1,
                stats: t3 - t2,
                algorithms: t4 - t3,
            },
        })
    }

    /// Processes a frame into a second output with the settings the last frame used, without
    /// statistics or algorithms (e.g. a half-size picture of the same frame).
    pub fn process_extra(
        &mut self,
        raw: &[u8],
        stride: usize,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<()> {
        self.isp.process(raw, stride, scale, out)?;
        Ok(())
    }
}
