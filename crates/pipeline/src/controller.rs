//! The 3A loop runner: one frame's statistics and what produced the frame in, the sensor
//! request (with the frame it lands on) and the ISP settings out.
//!
//! Deterministic: the same configuration, tuning, controls and per-frame inputs give the same
//! outputs bit for bit, so a recording made with [`Controller::record_to`] replays exactly with
//! `styx_algo::replay`.

use core::time::Duration;
#[cfg(feature = "std")]
use std::io::Write;

use serde::{Deserialize, Serialize};
#[cfg(feature = "std")]
use styx_algo::replay::Recorder;
use styx_algo::{
    CameraConfig, Controls, FrameMetadata, LensRequest, LensState, Params, Pipeline, SensorRequest,
    Statistics, Tuning, WarmStart,
};

use crate::error::Result;
use crate::isp::{IspSettings, lens_shading_with_bands};

/// What produced a frame, as the sensor side reports it (`styx-native`'s `FrameControls`:
/// predicted by the control schedule, or read back from the frame's embedded data).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SensorValues {
    /// Frame sequence number.
    pub frame: u64,
    /// Exposure time.
    pub exposure: Duration,
    /// Analogue gain.
    pub analogue_gain: f64,
    /// Sensor digital gain (1 without one).
    pub digital_gain: f64,
    /// Frame duration.
    pub frame_duration: Duration,
    /// Some values were read back from the frame rather than predicted.
    pub verified: bool,
}

impl SensorValues {
    /// Exposure × total sensor gain, in seconds.
    pub fn total_exposure(&self) -> f64 {
        self.exposure.as_secs_f64() * self.analogue_gain * self.digital_gain
    }
}

/// The loop's output for one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// The frame the statistics came from.
    pub frame: u64,
    /// A new sensor request (`None` when its values repeat the previous one's): apply it from
    /// `request.frame`.
    pub sensor: Option<SensorRequest>,
    /// A new lens position (`None` when it repeats the previous one), for cameras with a focus
    /// lens: apply it for `request.frame`.
    pub lens: Option<LensRequest>,
    /// ISP settings computed from this frame.
    pub isp: IspSettings,
    /// Everything the algorithms produced.
    pub params: Params,
}

/// The start-up values: the sensor request for frame 0 and the ISP settings before any
/// statistics.
#[derive(Clone, Debug, PartialEq)]
pub struct Start {
    /// Exposure, gain and frame duration to set before streaming.
    pub sensor: Option<SensorRequest>,
    /// The lens position to set before streaming (cameras with a focus lens).
    pub lens: Option<LensRequest>,
    /// ISP settings for the first frames.
    pub isp: IspSettings,
}

/// The 3A loop. See the [module documentation](self).
pub struct Controller {
    pipeline: Pipeline,
    config: CameraConfig,
    controls: Controls,
    max_digital_gain: f64,
    last_request: Option<SensorRequest>,
    last_lens: Option<i32>,
    #[cfg(feature = "std")]
    recorder: Option<Recorder<Box<dyn Write + Send>>>,
    /// A recording asked for before the start: its header names the warm start.
    #[cfg(feature = "std")]
    record_pending: Option<Box<dyn Write + Send>>,
    warm: Option<WarmStart>,
    started: bool,
    /// Scale of the spatial and colour denoise thresholds (see [`Self::set_spatial_denoise`]).
    spatial_denoise: f64,
    /// The params and settings of the last step the loop gave back ([`Self::recycle`]), whose
    /// buffers the next step's are copied into.
    spare: Option<(Params, IspSettings)>,
}

impl core::fmt::Debug for Controller {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Controller")
            .field("pipeline", &self.pipeline)
            .field("config", &self.config)
            .field("controls", &self.controls)
            .finish_non_exhaustive()
    }
}

impl Controller {
    /// The standard algorithms for `tuning`, prepared for `config`.
    pub fn new(tuning: &Tuning, config: CameraConfig) -> Result<Self> {
        let pipeline = Pipeline::from_tuning(tuning)?;
        config.validate()?;
        Ok(Self {
            pipeline,
            config,
            controls: Controls::default(),
            max_digital_gain: tuning.agc.as_ref().map_or(4.0, |a| a.max_digital_gain),
            last_request: None,
            last_lens: None,
            #[cfg(feature = "std")]
            recorder: None,
            #[cfg(feature = "std")]
            record_pending: None,
            warm: None,
            started: false,
            spatial_denoise: 1.0,
            spare: None,
        })
    }

    /// Scales the spatial (SDN) and colour (CDN) denoise the tuning asks for: their noise
    /// thresholds times `scale` (1, the default, as tuned; 0 turns both off). Applies to the
    /// ISP settings from the next frame on.
    pub fn set_spatial_denoise(&mut self, scale: f64) {
        self.spatial_denoise = scale.max(0.0);
    }

    /// Starts the next [`Self::start`] from an earlier session's settled values (`None`: from
    /// the tuning's start-up values).
    pub fn set_warm_start(&mut self, warm: Option<WarmStart>) {
        self.warm = warm;
    }

    /// The warm start the next (or current) session uses.
    pub fn warm_start(&self) -> Option<&WarmStart> {
        self.warm.as_ref()
    }

    /// What the algorithms have settled on, to start a later session from.
    pub fn warm_state(&self) -> Option<WarmStart> {
        self.started.then(|| self.pipeline.warm_state()).flatten()
    }

    /// Frames from a frame's statistics to the frame in which requests made from them are
    /// written (see [`styx_algo::ControlDelays::issue_latency`]); applies from the next
    /// [`Self::start`].
    pub fn set_issue_latency(&mut self, frames: u32) {
        self.config.delays.issue_latency = frames;
    }

    /// The camera configuration.
    pub fn config(&self) -> &CameraConfig {
        &self.config
    }

    /// Another camera configuration (e.g. another frame rate), from the next [`Self::start`].
    pub fn set_config(&mut self, config: CameraConfig) -> Result<()> {
        config.validate()?;
        self.config = config;
        Ok(())
    }

    /// The application controls applied from the next frame on.
    pub fn set_controls(&mut self, controls: Controls) {
        self.controls = controls;
    }

    /// Flicker avoidance from the next frame on (the other controls stay as they are).
    pub fn set_flicker(&mut self, flicker: styx_algo::Flicker) {
        self.controls.flicker = flicker;
    }

    /// The controls in effect.
    pub fn controls(&self) -> &Controls {
        &self.controls
    }

    /// Records every frame (statistics, metadata, output) as a `styx-algo` replay (feature
    /// `std`). Before the start the header is written at [`Self::start`] (with the
    /// configuration and warm start the algorithms are prepared with).
    #[cfg(feature = "std")]
    pub fn record_to(&mut self, out: impl Write + Send + 'static) -> Result<()> {
        let out: Box<dyn Write + Send> = Box::new(out);
        if self.started {
            self.recorder = Some(Recorder::with_warm_start(
                out,
                &self.config,
                self.warm.as_ref(),
            )?);
        } else {
            self.record_pending = Some(out);
        }
        Ok(())
    }

    /// Stops recording and flushes.
    #[cfg(feature = "std")]
    pub fn stop_recording(&mut self) -> Result<()> {
        if let Some(r) = self.recorder.take() {
            r.into_inner().flush()?;
        }
        if let Some(mut w) = self.record_pending.take() {
            w.flush()?;
        }
        Ok(())
    }

    /// Resets the algorithms (from the warm start, if one is set) and returns the start-up
    /// values.
    pub fn start(&mut self) -> Result<Start> {
        let warm = self.warm.filter(WarmStart::is_valid);
        let p = self
            .pipeline
            .prepare_warm(&self.config, warm.as_ref())?
            .clone();
        #[cfg(feature = "std")]
        if let Some(out) = self.record_pending.take() {
            self.recorder = Some(Recorder::with_warm_start(out, &self.config, warm.as_ref())?);
        }
        self.last_request = p.sensor;
        self.last_lens = p.lens.map(|l| l.position);
        self.started = true;
        Ok(Start {
            sensor: p.sensor,
            lens: p.lens,
            isp: IspSettings::from_params(&p, 0, 1.0).with_spatial_denoise(self.spatial_denoise),
        })
    }

    /// Runs the algorithms on frame `sensor.frame`'s statistics.
    pub fn process(&mut self, stats: &Statistics, sensor: &SensorValues) -> Result<Step> {
        self.process_with_lens(stats, sensor, None)
    }

    /// [`Self::process`] for a camera with a focus lens: `lens` is where the lens control
    /// reports the lens was during the frame.
    pub fn process_with_lens(
        &mut self,
        stats: &Statistics,
        sensor: &SensorValues,
        lens: Option<LensState>,
    ) -> Result<Step> {
        if !self.started {
            self.start()?;
        }
        let mut meta = self.meta(sensor);
        meta.lens = lens;
        let (mut params, mut isp) = self
            .spare
            .take()
            .unwrap_or_else(|| (Params::default(), IspSettings::neutral(0.0)));
        params.clone_from(self.pipeline.process(stats, &meta));
        #[cfg(feature = "std")]
        if let Some(r) = &mut self.recorder {
            r.record(stats, &meta, Some(&params))?;
        }
        // Only values that differ from the last request are news (AE repeats its request, with
        // a later landing frame, while nothing changes).
        let same = |a: &SensorRequest, b: &SensorRequest| {
            (a.exposure, a.analogue_gain, a.frame_duration)
                == (b.exposure, b.analogue_gain, b.frame_duration)
        };
        let sensor_request = params
            .sensor
            .filter(|r| self.last_request.is_none_or(|l| !same(r, &l)));
        if sensor_request.is_some() {
            self.last_request = params.sensor;
        }
        let lens = params.lens.filter(|l| self.last_lens != Some(l.position));
        if let Some(l) = lens {
            self.last_lens = Some(l.position);
        }
        self.isp_into(&mut isp, &params, sensor.frame, sensor);
        Ok(Step {
            frame: sensor.frame,
            sensor: sensor_request,
            lens,
            isp,
            params,
        })
    }

    /// ISP settings from `params` (computed from frame `from_frame`) for processing the frame
    /// `sensor` describes, with that frame's digital gain ([`Self::retarget`]).
    pub fn isp_for(&self, params: &Params, from_frame: u64, sensor: &SensorValues) -> IspSettings {
        let mut isp = IspSettings::neutral(0.0);
        self.isp_into(&mut isp, params, from_frame, sensor);
        isp
    }

    /// [`Self::isp_for`] into `isp`, reusing its buffers.
    fn isp_into(
        &self,
        isp: &mut IspSettings,
        params: &Params,
        from_frame: u64,
        sensor: &SensorValues,
    ) {
        isp.set_from_params(params, from_frame, 1.0);
        isp.scale_spatial_denoise(self.spatial_denoise);
        self.retarget(isp, params, sensor);
    }

    /// Gives a step the loop has finished with back, so that the next step's params and
    /// settings are copied into its buffers rather than new ones (no allocation per frame once
    /// they have grown).
    pub(crate) fn recycle(&mut self, step: Step) {
        self.spare = Some((step.params, step.isp));
    }

    /// Sets what in `isp` (made from `params`) belongs to the frame `sensor` describes, which
    /// may be a later frame than the one `params` came from: the digital gain is what the
    /// algorithms ask for in total divided by what the sensor delivered for that frame (so the
    /// image follows the target while a new exposure is still on its way, as the Raspberry
    /// Pi IPA does), between 1 and the tuning's maximum, divided by the brightness deflicker
    /// predicts for that frame (`Params::frame_gain`); on a rolling shutter deflicker's band
    /// gains go into the lens shading grid.
    pub fn retarget(&self, isp: &mut IspSettings, params: &Params, sensor: &SensorValues) {
        let g = params.frame_gain(&self.meta(sensor), self.max_digital_gain);
        isp.digital_gain = g.digital_gain * params.colour_gains[1].max(1e-6);
        isp.flicker = g.flicker;
        let bands = params.deflicker.as_ref().and_then(|d| {
            let rows = params
                .lens_shading
                .as_ref()
                .map_or(16, |l| l.height as usize);
            d.band_gains(
                sensor.frame,
                sensor.frame_duration.as_secs_f64(),
                sensor.exposure.as_secs_f64(),
                rows,
            )
        });
        if let Some(b) = &bands {
            isp.lens_shading = Some(lens_shading_with_bands(params.lens_shading.as_ref(), b));
        } else if isp.flicker_bands.is_some() {
            isp.lens_shading.clone_from(&params.lens_shading);
        }
        isp.flicker_bands = bands;
    }

    /// The digital gain [`Self::isp_for`] gives (before the white balance's green gain is
    /// folded in), without building the settings.
    pub fn digital_gain_for(&self, params: &Params, sensor: &SensorValues) -> f64 {
        params
            .frame_gain(&self.meta(sensor), self.max_digital_gain)
            .digital_gain
    }

    /// The algorithms' view of what produced a frame, with the controls in effect.
    fn meta(&self, sensor: &SensorValues) -> FrameMetadata {
        FrameMetadata {
            frame: sensor.frame,
            exposure: sensor.exposure,
            analogue_gain: sensor.analogue_gain,
            digital_gain: sensor.digital_gain,
            frame_duration: sensor.frame_duration,
            lux: None,
            lens: None,
            controls: self.controls.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use styx_algo::replay::{Recording, replay};
    use styx_algo::{StatsAccumulator, Tuning};

    use super::*;

    /// A grey scene at `level` (normalised, at 10 ms × 1).
    fn stats(level: f64, sensor: &SensorValues) -> Statistics {
        let k = level * sensor.total_exposure() / 0.01;
        let mut a = StatsAccumulator::new(4, 3, 64, 0.98);
        for zy in 0..3 {
            for zx in 0..4 {
                for _ in 0..20 {
                    let v = (k * (1.0 + 0.1 * f64::from(zx))).min(1.0);
                    a.add(zx, zy, v * 0.5, v, v * 0.7);
                }
            }
        }
        a.finish()
    }

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_camera_with_a_lens_gets_lens_moves() {
        let config = CameraConfig {
            lens: Some(styx_algo::LensConfig::default()),
            ..CameraConfig::default()
        };
        let mut c = Controller::new(&Tuning::default(), config).unwrap();
        // Start: the default position (1 D on the generic map: code 85).
        let start = c.start().unwrap();
        assert_eq!(start.lens.map(|l| l.position), Some(85));
        c.set_controls(Controls {
            lens_position: Some(3.0),
            ..Controls::default()
        });
        let mut sensor = SensorValues {
            frame: 0,
            exposure: Duration::from_millis(10),
            analogue_gain: 1.0,
            digital_gain: 1.0,
            frame_duration: Duration::from_millis(33),
            verified: false,
        };
        let lens = Some(LensState {
            position: 85.0,
            settled: true,
        });
        let step = c
            .process_with_lens(&stats(0.2, &sensor), &sensor, lens)
            .unwrap();
        let moved = step.lens.expect("a move");
        assert_eq!(moved.position, 256);
        // The same position again is not news.
        sensor.frame = 1;
        let step = c
            .process_with_lens(&stats(0.2, &sensor), &sensor, lens)
            .unwrap();
        assert!(step.lens.is_none());
        assert_eq!(step.params.af.lens_position, Some(3.0));
    }

    #[test]
    fn closed_loop_converges_lands_requests_and_replays() {
        let config = CameraConfig::default();
        let mut c = Controller::new(&Tuning::default(), config.clone()).unwrap();
        let rec = Shared::default();
        c.record_to(rec.clone()).unwrap();
        let start = c.start().unwrap();
        let first = start.sensor.unwrap();
        assert_eq!(first.frame, 0);
        // A sensor with the scheduler's model: values land on the requested frame.
        let mut pending = vec![first];
        let mut sensor = SensorValues {
            frame: 0,
            exposure: first.exposure,
            analogue_gain: first.analogue_gain,
            digital_gain: 1.0,
            frame_duration: first.frame_duration,
            verified: true,
        };
        let mut requests = 0;
        let mut last = None;
        for f in 0..60u64 {
            pending.sort_by_key(|r| r.frame);
            while let Some(r) = pending.first().copied().filter(|r| r.frame <= f) {
                sensor.exposure = r.exposure;
                sensor.analogue_gain = r.analogue_gain;
                sensor.frame_duration = r.frame_duration;
                pending.remove(0);
            }
            sensor.frame = f;
            let step = c.process(&stats(0.02, &sensor), &sensor).unwrap();
            if let Some(r) = step.sensor {
                assert_eq!(r.frame, config.delays.earliest_landing(f));
                pending.push(r);
                requests += 1;
            }
            assert!(step.isp.digital_gain >= 1.0);
            last = Some(step);
        }
        let last = last.unwrap();
        assert!(last.params.ae.locked, "{:?}", last.params.ae);
        assert!((1..40).contains(&requests), "{requests}");
        // Grey world takes out the 0.5 / 0.7 cast.
        assert!((last.isp.wb[0] - 2.0).abs() < 0.05, "{:?}", last.isp.wb);
        c.stop_recording().unwrap();
        let bytes = rec.0.lock().unwrap().clone();
        let recording = Recording::read(bytes.as_slice()).unwrap();
        assert_eq!(recording.records.len(), 60);
        let mut p = Pipeline::from_tuning(&Tuning::default()).unwrap();
        let report = replay(&mut p, &recording).unwrap();
        assert!(report.mismatches.is_empty());
    }

    /// The room's lamp (50 Hz mains 0.07 Hz off: ±25% at 50 Hz, ±10% at 100 Hz) over an
    /// exposure of `t` seconds ending at `end`.
    fn lamp(end: f64, t: f64) -> f64 {
        let part = |hz: f64, depth: f64| {
            let w = 2.0 * std::f64::consts::PI * hz;
            depth * ((w * end).sin() - (w * (end - t)).sin()) / (w * t)
        };
        1.0 + part(50.07, 0.25) + part(100.14, 0.1)
    }

    #[test]
    fn deflicker_gains_are_for_the_frame_being_processed() {
        // 120 fps, the PiSP path's order: frame F goes through the ISP with the settings made
        // from F - 1, retargeted to F; then F's statistics go through the algorithms.
        let fd = Duration::from_nanos(8_333_333);
        let config = CameraConfig {
            exposure_limits: (Duration::from_micros(20), Duration::from_micros(8100)),
            exposure_margin: Duration::from_micros(200),
            frame_duration_limits: (fd, fd),
            delays: styx_algo::ControlDelays {
                exposure: 2,
                analogue_gain: 2,
                frame_duration: 1,
                issue_latency: 1,
            },
            ..Default::default()
        };
        let mut c = Controller::new(&Tuning::default(), config).unwrap();
        c.set_flicker(styx_algo::Flicker::Auto);
        let start = c.start().unwrap();
        let first = start.sensor.unwrap();
        let mut pending = vec![first];
        let mut sensor = SensorValues {
            frame: 0,
            exposure: first.exposure,
            analogue_gain: first.analogue_gain,
            digital_gain: 1.0,
            frame_duration: fd,
            verified: true,
        };
        let mut isp = start.isp;
        let mut params: Option<Params> = None;
        let (mut out, mut stale, mut raw) = (Vec::new(), Vec::new(), Vec::new());
        for f in 0..1200u64 {
            pending.sort_by_key(|r| r.frame);
            while let Some(r) = pending.first().copied().filter(|r| r.frame <= f) {
                sensor.exposure = r.exposure;
                sensor.analogue_gain = r.analogue_gain;
                pending.remove(0);
            }
            let previous = sensor;
            sensor.frame = f;
            let end = (f + 1) as f64 * fd.as_secs_f64();
            let k = lamp(end, sensor.exposure.as_secs_f64());
            let level = 0.004 * k;
            if let Some(p) = &params {
                c.retarget(&mut isp, p, &sensor);
                let wrong = c.digital_gain_for(
                    p,
                    &SensorValues {
                        exposure: sensor.exposure,
                        ..previous
                    },
                );
                if f >= 600 {
                    let y = level * sensor.total_exposure() / 0.01;
                    out.push(y * isp.digital_gain);
                    stale.push(y * wrong * p.colour_gains[1]);
                    raw.push(y);
                    assert!(
                        (isp.flicker / k - 1.0).abs() < 0.01,
                        "{f}: {} vs {k}",
                        isp.flicker
                    );
                }
            }
            let step = c.process(&stats(level, &sensor), &sensor).unwrap();
            if let Some(r) = step.sensor {
                pending.push(r);
            }
            isp = step.isp;
            params = Some(step.params);
        }
        let sd = |v: &[f64]| {
            let m = v.iter().sum::<f64>() / v.len() as f64;
            (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt() / m
        };
        let (out, stale, raw) = (sd(&out), sd(&stale), sd(&raw));
        println!(
            "120 fps lamp: raw {raw:.4}, output {out:.4}, gain for the frame before {stale:.4}"
        );
        // The gain for the frame before (one frame of lag) would leave most of it in.
        assert!(
            raw > 0.08 && out < 0.005 && stale > raw / 2.0,
            "{raw} {out} {stale}"
        );
        assert!(params.unwrap().needs_every_frame());
    }

    #[test]
    fn band_gains_fold_into_lens_shading() {
        let ls = crate::isp::lens_shading_with_bands(None, &[1.1, 1.0, 0.9]);
        assert_eq!((ls.width, ls.height), (2, 3));
        assert_eq!(ls.g, vec![1.1, 1.1, 1.0, 1.0, 0.9, 0.9]);
        let grid = styx_algo::LensShading {
            width: 1,
            height: 6,
            r: vec![2.0; 6],
            g: vec![1.0; 6],
            b: vec![1.0; 6],
        };
        let ls = crate::isp::lens_shading_with_bands(Some(&grid), &[1.2, 0.8]);
        assert!((ls.r[0] - 2.4).abs() < 1e-12 && (ls.r[5] - 1.6).abs() < 1e-12);
        assert!(ls.g[0] > ls.g[2] && ls.g[2] > ls.g[3] && ls.g[3] > ls.g[5]);
    }
}
