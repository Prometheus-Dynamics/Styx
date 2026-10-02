//! The 3A loop with the software ISP: `styx-softisp` processes each raw frame and gathers its
//! statistics in the same pass, the controller turns them into a sensor request and ISP
//! settings, and those settings process the next frame.

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
    /// The frame's statistics.
    pub stats: Statistics,
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
}

impl std::fmt::Debug for SoftLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftLoop")
            .field("info", &self.info)
            .field("isp", &self.isp)
            .finish_non_exhaustive()
    }
}

/// Software ISP parameters the loop does not change: bilinear demosaic, full-range BT.601 YUV
/// (as the PiSP back end's "jpeg" encoding), statistics on a 16x12 zone grid with a 256-bin
/// histogram (every second quad row).
pub fn base_params() -> IspParams {
    IspParams {
        demosaic: Demosaic::Bilinear,
        yuv: YuvMatrix::Bt601Full,
        stats: Some(StatsConfig {
            zones_x: 16,
            zones_y: 12,
            histogram_bins: 256,
            saturation: 0.95,
            row_step: 2,
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
        let isp = SoftIsp::new(format, start.softisp(info.bits, &base))?.with_threads(threads);
        Ok(Self {
            info,
            controller,
            isp,
            base,
            latest: None,
            start,
            applied: None,
        })
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
        let settings = self.settings_for(sensor);
        if self.applied.as_ref() != Some(&settings) {
            self.isp
                .set_params(settings.softisp(self.info.bits, &self.base))?;
            self.applied = Some(settings.clone());
        }
        let raw_stats = self.isp.process(raw, stride, scale, out)?;
        let stats = raw_stats
            .map(|s| {
                stats::from_softisp(&s, self.info.black_level, settings.lens_shading.is_some())
            })
            .unwrap_or_default();
        let step = self.controller.process(&stats, sensor)?;
        self.latest = Some((step.params.clone(), step.frame));
        Ok(SoftOutput {
            step,
            applied: settings,
            stats,
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
