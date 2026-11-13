//! The loop runner: one frame's statistics and metadata in, the sensor request (with the frame
//! it lands on), the lens move and the neutral ISP settings out.

use styx_aaa::{
    CameraConfig, FrameMetadata, LensRequest, Params, Pipeline, SensorRequest, Statistics, Tuning,
    WarmStart,
};

use crate::error::Result;
use crate::isp::{IspSettings, lens_shading_with_bands};
use crate::sensor::SensorUpdates;

/// The start-up values: the sensor request for the first frames and the ISP settings before any
/// statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    /// Exposure, gain and frame duration to set before streaming.
    pub sensor: Option<SensorRequest>,
    /// The same as controls, for the frame the request lands on.
    pub updates: Option<SensorUpdates>,
    /// The lens position to set before streaming (cameras with a focus lens).
    pub lens: Option<LensRequest>,
    /// ISP settings for the first frames.
    pub isp: IspSettings,
}

/// The loop's output for one frame. The settings and parameters borrow the controller's own
/// buffers, so a frame allocates nothing once the buffers have grown.
#[derive(Debug, Clone, Copy)]
pub struct Step<'a> {
    /// The frame the statistics came from.
    pub frame: u64,
    /// A new sensor request (`None` when its values repeat the previous one's): apply it from
    /// `request.frame`.
    pub sensor: Option<SensorRequest>,
    /// The same request as controls, each for the frame it lands on (`Some` exactly when
    /// `sensor` is).
    pub updates: Option<SensorUpdates>,
    /// A new lens position (`None` when it repeats the previous one), for cameras with a focus
    /// lens: apply it for `request.frame`.
    pub lens: Option<LensRequest>,
    /// The neutral ISP settings for this frame.
    pub isp: &'a IspSettings,
    /// Everything the algorithms produced.
    pub params: &'a Params,
}

/// The 3A loop over one [`Pipeline`] built from a tuning. See the [crate documentation](crate).
#[derive(Debug)]
pub struct Controller {
    pipeline: Pipeline,
    config: CameraConfig,
    max_digital_gain: f64,
    /// Scale of the spatial and colour denoise thresholds (see [`Self::set_spatial_denoise`]).
    spatial_denoise: f64,
    warm: Option<WarmStart>,
    started: bool,
    last_request: Option<SensorRequest>,
    last_lens: Option<i32>,
    /// The settings of the last frame, whose buffers the next frame's are copied into.
    isp: IspSettings,
}

impl Controller {
    /// The standard algorithms for `tuning`, prepared for `config` at the next [`Self::start`].
    pub fn new(tuning: &Tuning, config: CameraConfig) -> Result<Self> {
        let pipeline = Pipeline::from_tuning(tuning)?;
        config.validate()?;
        Ok(Self {
            pipeline,
            config,
            max_digital_gain: tuning.agc.as_ref().map_or(4.0, |a| a.max_digital_gain),
            spatial_denoise: 1.0,
            warm: None,
            started: false,
            last_request: None,
            last_lens: None,
            isp: IspSettings::neutral(0.0),
        })
    }

    /// The camera configuration.
    pub fn config(&self) -> &CameraConfig {
        &self.config
    }

    /// Scales the spatial (SDN) and colour (CDN) denoise the tuning asks for: their thresholds
    /// times `scale` (1, the default, as tuned; 0 turns both off). From the next frame on.
    pub fn set_spatial_denoise(&mut self, scale: f64) {
        self.spatial_denoise = scale.max(0.0);
    }

    /// Starts the next [`Self::start`] from an earlier session's settled values (`None`: from
    /// the tuning's start-up values).
    pub fn set_warm_start(&mut self, warm: Option<WarmStart>) {
        self.warm = warm;
    }

    /// What the algorithms have settled on, to start a later session from (`None` before the
    /// loop has started or before AE produced a request).
    pub fn warm_state(&self) -> Option<WarmStart> {
        self.started.then(|| self.pipeline.warm_state()).flatten()
    }

    /// Resets the algorithms (from the warm start, if one is set) and returns the start-up
    /// values.
    pub fn start(&mut self) -> Result<Start> {
        let warm = self.warm.filter(WarmStart::is_valid);
        let p = self
            .pipeline
            .prepare_warm(&self.config, warm.as_ref())?
            .clone();
        let updates = p
            .sensor
            .as_ref()
            .map(SensorUpdates::from_request)
            .transpose()?;
        self.last_request = p.sensor;
        self.last_lens = p.lens.map(|l| l.position);
        self.started = true;
        self.isp.set_from_params(&p, 0, 1.0);
        self.isp.scale_spatial_denoise(self.spatial_denoise);
        Ok(Start {
            sensor: p.sensor,
            updates,
            lens: p.lens,
            isp: self.isp.clone(),
        })
    }

    /// Runs the algorithms on one frame's statistics. `meta` is what produced the frame and the
    /// controls in force for it. The loop starts itself on the first frame.
    pub fn process(&mut self, stats: &Statistics, meta: &FrameMetadata) -> Result<Step<'_>> {
        if !self.started {
            self.start()?;
        }
        let params = self.pipeline.process(stats, meta);
        // Only values that differ from the last request are news (AE repeats its request, with a
        // later landing frame, while nothing changes).
        let same = |a: &SensorRequest, b: &SensorRequest| {
            (a.exposure, a.analogue_gain, a.frame_duration)
                == (b.exposure, b.analogue_gain, b.frame_duration)
        };
        let sensor = params
            .sensor
            .filter(|r| self.last_request.is_none_or(|l| !same(r, &l)));
        let updates = sensor
            .as_ref()
            .map(SensorUpdates::from_request)
            .transpose()?;
        if sensor.is_some() {
            self.last_request = params.sensor;
        }
        let lens = params.lens.filter(|l| self.last_lens != Some(l.position));
        if let Some(l) = lens {
            self.last_lens = Some(l.position);
        }
        settings_for_frame(
            &mut self.isp,
            params,
            meta,
            self.max_digital_gain,
            self.spatial_denoise,
        );
        Ok(Step {
            frame: meta.frame,
            sensor,
            updates,
            lens,
            isp: &self.isp,
            params,
        })
    }
}

/// The settings for the frame `meta` describes: what `params` (computed from it) asks for, with
/// that frame's digital gain (see `Params::frame_gain`), and deflicker's correction for it. On a
/// rolling shutter, deflicker's gains per band of rows go into the lens shading grid.
fn settings_for_frame(
    isp: &mut IspSettings,
    params: &Params,
    meta: &FrameMetadata,
    max_digital_gain: f64,
    spatial_denoise: f64,
) {
    isp.set_from_params(params, meta.frame, 1.0);
    isp.scale_spatial_denoise(spatial_denoise);
    let g = params.frame_gain(meta, max_digital_gain);
    isp.digital_gain = g.digital_gain * params.colour_gains[1].max(1e-6);
    isp.flicker = g.flicker;
    let bands = params.deflicker.as_ref().and_then(|d| {
        let rows = params
            .lens_shading
            .as_ref()
            .map_or(16, |l| l.height as usize);
        d.band_gains(
            meta.frame,
            meta.frame_duration.as_secs_f64(),
            meta.exposure.as_secs_f64(),
            rows,
        )
    });
    if let Some(b) = &bands {
        isp.lens_shading = Some(lens_shading_with_bands(params.lens_shading.as_ref(), b));
    }
    isp.flicker_bands = bands;
}
