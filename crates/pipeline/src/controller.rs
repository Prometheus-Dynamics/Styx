//! The 3A loop runner: one frame's statistics and what produced the frame in, the sensor
//! request (with the frame it lands on) and the ISP settings out.
//!
//! Deterministic: the same configuration, tuning, controls and per-frame inputs give the same
//! outputs bit for bit, so a recording made with [`Controller::record_to`] replays exactly with
//! `styx_algo::replay`.

use std::io::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use styx_algo::replay::Recorder;
use styx_algo::{
    CameraConfig, Controls, FrameMetadata, Params, Pipeline, SensorRequest, Statistics, Tuning,
    WarmStart,
};

use crate::error::Result;
use crate::isp::IspSettings;

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
    recorder: Option<Recorder<Box<dyn Write + Send>>>,
    /// A recording asked for before the start: its header names the warm start.
    record_pending: Option<Box<dyn Write + Send>>,
    warm: Option<WarmStart>,
    started: bool,
    /// Scale of the spatial and colour denoise thresholds (see [`Self::set_spatial_denoise`]).
    spatial_denoise: f64,
}

impl std::fmt::Debug for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
            recorder: None,
            record_pending: None,
            warm: None,
            started: false,
            spatial_denoise: 1.0,
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

    /// Records every frame (statistics, metadata, output) as a `styx-algo` replay. Before the
    /// start the header is written at [`Self::start`] (with the configuration and warm start
    /// the algorithms are prepared with).
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
        if let Some(out) = self.record_pending.take() {
            self.recorder = Some(Recorder::with_warm_start(out, &self.config, warm.as_ref())?);
        }
        self.last_request = p.sensor;
        self.started = true;
        Ok(Start {
            sensor: p.sensor,
            isp: IspSettings::from_params(&p, 0, 1.0).with_spatial_denoise(self.spatial_denoise),
        })
    }

    /// Runs the algorithms on frame `sensor.frame`'s statistics.
    pub fn process(&mut self, stats: &Statistics, sensor: &SensorValues) -> Result<Step> {
        if !self.started {
            self.start()?;
        }
        let meta = FrameMetadata {
            frame: sensor.frame,
            exposure: sensor.exposure,
            analogue_gain: sensor.analogue_gain,
            digital_gain: sensor.digital_gain,
            frame_duration: sensor.frame_duration,
            lux: None,
            controls: self.controls.clone(),
        };
        let params = self.pipeline.process(stats, &meta).clone();
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
        Ok(Step {
            frame: sensor.frame,
            sensor: sensor_request,
            isp: self.isp_for(&params, sensor.frame, sensor),
            params,
        })
    }

    /// ISP settings from `params` (computed from frame `from_frame`) for processing the frame
    /// `sensor` describes. The digital gain is what the algorithms ask for in total divided
    /// by what the sensor delivered for that frame (so the image follows the target while a
    /// new exposure is still on its way, as the Raspberry Pi IPA does), between 1 and the
    /// tuning's maximum.
    pub fn isp_for(&self, params: &Params, from_frame: u64, sensor: &SensorValues) -> IspSettings {
        IspSettings::from_params(params, from_frame, self.digital_gain_for(params, sensor))
            .with_spatial_denoise(self.spatial_denoise)
    }

    /// The digital gain [`Self::isp_for`] gives (before the white balance's green gain is
    /// folded in), without building the settings.
    pub fn digital_gain_for(&self, params: &Params, sensor: &SensorValues) -> f64 {
        let delivered = sensor.total_exposure();
        if !self.controls.ae_enable || params.ae.total_exposure <= 0.0 || delivered <= 0.0 {
            params.digital_gain.max(1.0)
        } else {
            (params.ae.total_exposure / delivered).clamp(1.0, self.max_digital_gain)
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
}
