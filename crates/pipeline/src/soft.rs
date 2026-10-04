//! The 3A loop with the software ISP: `styx-softisp` processes each raw frame and gathers its
//! statistics in the same pass, the controller turns them into a sensor request and ISP
//! settings, and those settings process the next frame.

#[cfg(not(feature = "std"))]
use crate::math::Float as _;
#[cfg(feature = "gpu")]
use alloc::boxed::Box;
use core::time::Duration;

use crate::time::now;

use styx_algo::{LensState, Params, PdafZone, Statistics, Tuning, ZoneGrid};
use styx_softisp::{
    Demosaic, IspParams, OutputBuffers, RawFormat, RawPacking, Scale, SoftIsp, StatsConfig,
    YuvMatrix,
};

use crate::engine::{Engine, IspEngine, RawFrame};

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
    pub settings: Duration,
    /// The software ISP: the picture and its statistics.
    pub isp: Duration,
    /// Converting the statistics for the algorithms.
    pub stats: Duration,
    /// The algorithms (AE, AWB, lens shading, colour, tone).
    pub algorithms: Duration,
}

/// The software ISP loop. See the [module documentation](self).
pub struct SoftLoop {
    info: SensorInfo,
    controller: Controller,
    isp: Engine,
    base: IspParams,
    /// The latest algorithm output and the frame it came from.
    latest: Option<(Params, u64)>,
    start: IspSettings,
    applied: Option<IspSettings>,
    /// While settled, statistics and algorithms run every this many frames.
    settled_every: u64,
    /// The frame the algorithms last ran on.
    last_run: Option<u64>,
    /// Where the lens was for the next frame, and its phase detection data.
    next_focus: (Option<LensState>, Option<ZoneGrid<PdafZone>>),
}

impl core::fmt::Debug for SoftLoop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SoftLoop")
            .field("info", &self.info)
            .field("isp", &self.isp.kind())
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
            ..StatsConfig::default()
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
        let mut base = base_params();
        // A camera with a focus lens gets focus statistics for AF.
        if info.camera.lens.is_some()
            && let Some(s) = base.stats.as_mut()
        {
            s.focus = true;
        }
        let isp = SoftIsp::new(format, start.softisp(info.bits, &base))?;
        let mut isp = crate::engine::with_threads(isp, threads);
        isp.set_lens_shading_tolerance(LSC_TOLERANCE);
        Ok(Self {
            info,
            controller,
            isp: Engine::Cpu(isp),
            base,
            latest: None,
            start,
            applied: None,
            settled_every: 1,
            last_run: None,
            next_focus: (None, None),
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

    /// Whether raw rows are copied into a cached buffer before unpacking (default: yes, for
    /// buffers the CPU maps uncached; frames in cached memory need no copy). See
    /// [`SoftIsp::set_copy_input`].
    pub fn set_copy_input(&mut self, copy: bool) {
        self.isp.set_copy_input(copy);
    }

    /// Which ISP processes the frames.
    pub fn engine(&self) -> IspEngine {
        self.isp.kind()
    }

    /// The GPU's time on the last frame (GPU ISP on a device with timestamp queries).
    pub fn gpu_time(&self) -> Option<Duration> {
        self.isp.gpu_time()
    }

    /// Process frames on `context`'s GPU from now on (`styx-gpuisp`: the same parameters,
    /// pictures and statistics as the integer arithmetic of the software ISP). The loop's
    /// state is kept; the next frame's settings are applied afresh.
    #[cfg(feature = "gpu")]
    pub fn use_gpu(&mut self, context: &styx_gpuisp::GpuContext) -> Result<()> {
        let params = self.isp.params().clone();
        let isp = styx_gpuisp::GpuIsp::with_context(context, self.isp.format(), params)?;
        let copy = match &self.isp {
            Engine::Cpu(_) => true,
            Engine::Gpu(g) => g.copy_input,
        };
        self.isp = Engine::Gpu(Box::new(crate::engine::gpu::Gpu::new(isp, copy)));
        self.applied = None;
        Ok(())
    }

    /// Process frames with `styx-softisp` on `threads` threads (after [`Self::use_gpu`]).
    #[cfg(feature = "gpu")]
    pub fn use_cpu(&mut self, threads: usize) -> Result<()> {
        if let Engine::Cpu(i) = &mut self.isp {
            i.set_threads(threads);
            return Ok(());
        }
        let params = self.isp.params().clone();
        let mut isp = SoftIsp::new(self.isp.format(), params)?.with_threads(threads);
        isp.set_lens_shading_tolerance(LSC_TOLERANCE);
        if let Engine::Gpu(g) = &self.isp {
            isp.set_copy_input(g.copy_input);
        }
        self.isp = Engine::Cpu(isp);
        self.applied = None;
        Ok(())
    }

    /// The controller (controls, recording).
    pub fn controller(&mut self) -> &mut Controller {
        &mut self.controller
    }

    /// The sensor mode.
    pub fn info(&self) -> &SensorInfo {
        &self.info
    }

    /// The arithmetic the ISP ran the last frame with ([`styx_softisp::Arithmetic::Auto`]
    /// resolved, see [`SoftIsp::arithmetic`]).
    pub fn arithmetic(&self) -> styx_softisp::Arithmetic {
        self.isp.arithmetic()
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
        self.isp.forget_imports();
        self.latest = None;
        self.applied = None;
        self.last_run = None;
        Ok(s)
    }

    /// Where the focus lens was for the next frame to be processed, and the frame's phase
    /// detection data (cameras with a lens; see `styx_algo::FrameMetadata::lens`).
    pub fn set_frame_focus(&mut self, lens: Option<LensState>, pdaf: Option<ZoneGrid<PdafZone>>) {
        self.next_focus = (lens, pdaf);
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
        self.process_frame(RawFrame::Bytes(raw), stride, sensor, scale, out)
    }

    /// [`Self::process`] from any [`RawFrame`] (a dma-buf with the GPU ISP).
    pub fn process_frame(
        &mut self,
        raw: RawFrame<'_>,
        stride: usize,
        sensor: &SensorValues,
        scale: Scale,
        out: OutputBuffers<'_>,
    ) -> Result<SoftOutput> {
        let t0 = now();
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
                .is_some_and(|(p, _)| p.ae.locked && p.awb.converged && !p.needs_every_frame());
        let run = !settled
            || self
                .last_run
                .is_none_or(|r| sensor.frame >= r + self.settled_every);
        self.isp.set_statistics(run);
        let t1 = now();
        let raw_stats = self.isp.process(raw, stride, scale, out)?;
        let t2 = now();
        let mut stats = raw_stats
            .map(|s| {
                stats::from_softisp(&s, self.info.black_level, settings.lens_shading.is_some())
            })
            .unwrap_or_default();
        let (lens, pdaf) = core::mem::take(&mut self.next_focus);
        stats.pdaf = pdaf;
        let t3 = now();
        let step = match (&self.latest, run) {
            (Some((params, _)), false) => Step {
                frame: sensor.frame,
                sensor: None,
                lens: None,
                isp: settings.clone(),
                params: params.clone(),
            },
            _ => {
                let step = self.controller.process_with_lens(&stats, sensor, lens)?;
                self.latest = Some((step.params.clone(), step.frame));
                self.last_run = Some(sensor.frame);
                step
            }
        };
        let t4 = now();
        Ok(SoftOutput {
            step,
            applied: settings,
            stats,
            timing: SoftTiming {
                settings: t1.since(t0),
                isp: t2.since(t1),
                stats: t3.since(t2),
                algorithms: t4.since(t3),
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
        self.isp.process(RawFrame::Bytes(raw), stride, scale, out)?;
        Ok(())
    }
}
