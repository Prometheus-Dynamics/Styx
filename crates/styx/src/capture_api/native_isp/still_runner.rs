//! Still requests on a processed native capture's worker. The decisions (which frame's raw
//! data to keep, fixed and bracketed exposures on the control schedule, AE handed back,
//! timeouts) are the pipeline core's platform-neutral [`styx_pipeline::still_runner`]; this is
//! the Linux side around them: requests from the control plane ([`LoopControls`]), the
//! targets shared with the PiSP pipeline's raw copy hook, and held frames handed to the
//! [`super::still_process`] thread, so the stream never waits for a still. The worker calls
//! [`StillRunner::before_frame`] and [`StillRunner::after_frame`] around each frame.

use std::sync::{Arc, mpsc};
use std::time::Instant;

use parking_lot::Mutex;
use styx_pipeline::SensorValues;
use styx_pipeline::still::HeldRaw;
use styx_pipeline::still_runner::{
    self as decide, LoopReport, ShotExposure, StillFailure, StillOrder, StillOutcome, Target,
};
use styx_pipeline::styx_algo::{CameraConfig, Controls, SensorRequest};

use super::super::request::CaptureError;
use super::super::still::{StillCapture, StillExposure, StillRequest};
use super::LoopControls;
use super::still_process::{Batch, StillProcessor};

/// A still request on its way to the worker.
pub(crate) struct StillJob {
    pub request: StillRequest,
    pub reply: mpsc::Sender<Result<StillCapture, CaptureError>>,
    pub requested: Instant,
}

impl std::fmt::Debug for StillJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StillJob")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// What the 3A loop is driven through (the PiSP or the software pipeline).
pub(crate) trait StillHost {
    fn set_controls(&mut self, c: Controls);
    fn camera(&mut self) -> CameraConfig;
}

impl StillHost for styx_pipeline::device::PispPipeline {
    fn set_controls(&mut self, c: Controls) {
        self.controller().set_controls(c);
    }

    fn camera(&mut self) -> CameraConfig {
        self.info().camera.clone()
    }
}

impl StillHost for styx_pipeline::device::SoftPipeline {
    fn set_controls(&mut self, c: Controls) {
        self.soft_loop().controller().set_controls(c);
    }

    fn camera(&mut self) -> CameraConfig {
        self.soft_loop().info().camera.clone()
    }
}

/// The loop as the decisions drive it: the application's controls are the control plane's.
struct Host<'a> {
    host: &'a mut dyn StillHost,
    loop_controls: &'a LoopControls,
}

impl decide::StillHost for Host<'_> {
    fn set_controls(&mut self, c: Controls) {
        self.host.set_controls(c);
    }

    fn controls(&mut self) -> Controls {
        self.loop_controls.current()
    }

    fn camera(&mut self) -> CameraConfig {
        self.host.camera()
    }
}

/// The frames whose raw data to keep, shared with the PiSP pipeline's copy hook.
pub(crate) type Targets = Arc<Mutex<Vec<Target>>>;

/// See the [module documentation](self).
pub(crate) struct StillRunner {
    loop_controls: Arc<LoopControls>,
    decide: decide::StillRunner<StillJob, Box<HeldRaw>>,
    targets: Targets,
    generation: u64,
    processor: Option<StillProcessor>,
    spawn: Box<dyn FnMut() -> StillProcessor + Send>,
    /// The decisions' clock: time since the runner was made.
    epoch: Instant,
}

impl StillRunner {
    pub(crate) fn new(
        loop_controls: Arc<LoopControls>,
        spawn: Box<dyn FnMut() -> StillProcessor + Send>,
    ) -> Self {
        Self {
            loop_controls,
            decide: decide::StillRunner::new(),
            targets: Arc::new(Mutex::new(Vec::new())),
            generation: 0,
            processor: None,
            spawn,
            epoch: Instant::now(),
        }
    }

    /// The frames to copy (for the PiSP pipeline's copy hook).
    pub(crate) fn targets(&self) -> Targets {
        Arc::clone(&self.targets)
    }

    /// Whether frame `s`'s raw data is wanted.
    pub(crate) fn wants(&self, s: &SensorValues) -> bool {
        self.decide.wants(s)
    }

    fn order(&self, job: &StillJob) -> StillOrder {
        let r = &job.request;
        StillOrder {
            exposure: match &r.exposure {
                StillExposure::Current => ShotExposure::Current,
                StillExposure::Fixed { exposure, gain } => ShotExposure::Fixed {
                    exposure: *exposure,
                    gain: *gain,
                },
                StillExposure::Bracket(evs) => ShotExposure::Bracket(evs.clone()),
            },
            settle: r.settle,
            timeout: r.timeout,
            requested: job.requested.saturating_duration_since(self.epoch),
        }
    }

    /// The shared targets follow the decisions' (only when they changed: no lock per frame).
    fn sync_targets(&mut self) {
        if self.decide.generation() != self.generation {
            self.generation = self.decide.generation();
            let mut t = self.targets.lock();
            t.clear();
            t.extend_from_slice(self.decide.targets());
        }
    }

    fn finish(&mut self, outcome: Option<StillOutcome<StillJob, Box<HeldRaw>>>) {
        match outcome {
            None => {}
            Some(StillOutcome::Failed { job, reason }) => {
                let why = match reason {
                    StillFailure::TimedOut => "still: timed out",
                };
                let _ = job.reply.send(Err(CaptureError::Backend(why.into())));
            }
            Some(StillOutcome::Taken { job, shots }) if shots.is_empty() => {
                let _ = job.reply.send(Ok(StillCapture::default()));
            }
            Some(StillOutcome::Taken { job, shots }) => {
                let p = self.processor.get_or_insert_with(&mut self.spawn);
                p.submit(Batch { job, shots });
            }
        }
        self.sync_targets();
    }

    /// Before the worker asks for the next frame: picks up new requests and starts one.
    pub(crate) fn before_frame(&mut self, host: &mut dyn StillHost) {
        for job in self.loop_controls.take_stills() {
            let order = self.order(&job);
            self.decide.submit(job, order);
        }
        if self.decide.is_idle() {
            return;
        }
        let now = self.epoch.elapsed();
        let mut host = Host {
            host,
            loop_controls: &self.loop_controls,
        };
        let outcome = self.decide.before_frame(&mut host, now);
        self.finish(outcome);
    }

    /// After frame `s`: `lands` is where the request made from its statistics lands
    /// (`request` the request), `total` AE's total exposure, `raw` the frame's raw data if it
    /// was wanted.
    pub(crate) fn after_frame(
        &mut self,
        host: &mut dyn StillHost,
        s: &SensorValues,
        (lands, request): (Option<u64>, Option<SensorRequest>),
        (total, ae_locked): (f64, bool),
        raw: Option<Box<HeldRaw>>,
    ) {
        let report = LoopReport {
            lands,
            request,
            total_exposure: total,
            ae_locked,
        };
        let mut host = Host {
            host,
            loop_controls: &self.loop_controls,
        };
        let outcome = self.decide.after_frame(&mut host, s, &report, raw);
        if outcome.is_some() || self.decide.generation() != self.generation {
            self.finish(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn cam() -> CameraConfig {
        CameraConfig {
            exposure_limits: (Duration::from_micros(20), Duration::from_millis(100)),
            exposure_margin: Duration::from_micros(500),
            frame_duration_limits: (Duration::from_millis(33), Duration::from_millis(33)),
            analogue_gain_limits: (1.0, 16.0),
            ..CameraConfig::default()
        }
    }

    /// The 3A loop as the still runner sees it: controls handed over, AE asking for them
    /// (landing three frames later), frames produced with what was asked.
    struct FakeLoop {
        controls: Vec<Controls>,
    }

    impl StillHost for FakeLoop {
        fn set_controls(&mut self, c: Controls) {
            self.controls.push(c);
        }

        fn camera(&mut self) -> CameraConfig {
            cam()
        }
    }

    fn held(s: &SensorValues) -> Box<HeldRaw> {
        use styx_pipeline::IspSettings;
        let (w, h) = (8u32, 4u32);
        Box::new(HeldRaw {
            sequence: s.frame,
            timestamp: Duration::from_millis(s.frame * 33),
            width: w,
            height: h,
            stride: w as usize * 2,
            packing: styx_softisp::RawPacking::U16Le { bits: 10 },
            cfa: styx_softisp::CfaPattern::Bggr,
            bits: 10,
            data: (0..w * h).flat_map(|_| 300u16.to_le_bytes()).collect(),
            sensor: *s,
            isp: IspSettings::neutral(64.0 / 1024.0),
            params: Box::default(),
        })
    }

    #[test]
    fn a_bracket_lands_on_the_frames_the_schedule_names() {
        use super::super::super::still::StillFormat;
        use super::super::still_process::StillContext;
        let lc = Arc::new(LoopControls::default());
        let ctx = Arc::new(StillContext {
            kind: styx_pipeline::device::IspKind::Software,
            source: Default::default(),
            threads: 1,
        });
        let mut runner = StillRunner::new(
            Arc::clone(&lc),
            Box::new(move || StillProcessor::spawn(Arc::clone(&ctx))),
        );
        let mut host = FakeLoop {
            controls: Vec::new(),
        };
        let (tx, rx) = mpsc::channel();
        lc.submit_still(StillJob {
            request: StillRequest::format(StillFormat::Rgb24)
                .with_dng(true)
                .bracket([-1.0, 0.0, 1.0]),
            reply: tx,
            requested: Instant::now(),
        });
        // AE at 10 ms x 2 (total 0.02) when the request arrives.
        let mut sensor = SensorValues {
            frame: 0,
            exposure: Duration::from_millis(10),
            analogue_gain: 2.0,
            digital_gain: 1.0,
            frame_duration: Duration::from_millis(33),
            verified: true,
        };
        // Requests made at frame f land at f + 3; frames show what landed.
        let mut landing: Vec<(u64, (Duration, f64))> = Vec::new();
        for f in 0..20u64 {
            runner.before_frame(&mut host);
            sensor.frame = f;
            if let Some(&(_, (e, g))) = landing.iter().rev().find(|(at, _)| *at <= f) {
                (sensor.exposure, sensor.analogue_gain) = (e, g);
            }
            let raw = runner.wants(&sensor).then(|| held(&sensor));
            // AE asks for the last controls handed over (or AE's own 10 ms x 2).
            let c = host.controls.last().cloned().unwrap_or_default();
            let want = match (c.exposure, c.analogue_gain) {
                (Some(e), Some(g)) => (e, g),
                _ => (Duration::from_millis(10), 2.0),
            };
            let request = SensorRequest {
                frame: f + 3,
                exposure: want.0,
                analogue_gain: want.1,
                frame_duration: Duration::from_millis(33),
            };
            landing.push((f + 3, want));
            runner.after_frame(
                &mut host,
                &sensor,
                (Some(f + 3), Some(request)),
                (0.02, true),
                raw,
            );
        }
        let still = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(still.shots.len(), 3);
        let frames: Vec<u64> = still.shots.iter().map(|s| s.meta.sequence).collect();
        // Started after frame 0, requested at frames 1, 2, 3: consecutive frames 4, 5, 6.
        assert_eq!(frames, [4, 5, 6]);
        for (s, ev) in still.shots.iter().zip([-1.0, 0.0, 1.0]) {
            assert!(s.meta.landed, "{:?}", s.meta);
            assert_eq!(s.meta.ev, ev);
            let total = s.meta.exposure.as_secs_f64() * s.meta.analogue_gain;
            assert!(
                (total / (0.02 * 2f64.powf(ev)) - 1.0).abs() < 1e-6,
                "{total}"
            );
            assert_eq!(s.image.as_ref().unwrap().data.len(), 8 * 4 * 3);
            let dng = styx_dng::read_dng(s.dng.as_ref().unwrap()).unwrap();
            assert_eq!(dng.samples[0], 300);
        }
        // AE got the application's controls back after the last exposure.
        let last = host.controls.last().unwrap();
        assert_eq!((last.exposure, last.analogue_gain), (None, None));
        assert!(runner.targets.lock().is_empty());
    }
}
