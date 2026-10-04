//! Still requests on a processed native capture's worker: which frames' raw data to keep
//! (the next one, the first after AE locks, or the frame a fixed or bracketed exposure lands
//! on, as the control schedule says), driving the 3A loop's controls for fixed and bracketed
//! exposures and handing them back afterwards. The worker calls [`StillRunner::before_frame`]
//! and [`StillRunner::after_frame`] around each frame; held frames go to the
//! [`super::still_process`] thread, so the stream never waits for a still.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_pipeline::SensorValues;
use styx_pipeline::still::HeldRaw;
use styx_pipeline::styx_algo::{CameraConfig, Controls, SensorRequest};

use super::super::request::CaptureError;
use super::super::still::{StillCapture, StillExposure, StillRequest};
use super::LoopControls;
use super::still_process::{Batch, HeldShot, StillProcessor};

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

/// A frame the stream wants the raw data of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Target {
    /// From this frame on.
    frame: u64,
    /// With this exposure and gain (or any frame after `frame + GIVE_UP`).
    want: Option<(Duration, f64)>,
}

/// Frames after its target a shot waits for its exposure before taking what comes.
const GIVE_UP: u64 = 4;

pub(crate) fn matches(want: (Duration, f64), s: &SensorValues) -> bool {
    let (t, g) = (want.0.as_secs_f64(), want.1);
    let e = s.exposure.as_secs_f64();
    (e - t).abs() <= (0.03 * t).max(60e-6) && (s.analogue_gain - g).abs() <= 0.03 * g
}

impl Target {
    pub(crate) fn wants(&self, s: &SensorValues) -> bool {
        s.frame >= self.frame
            && (self.want.is_none_or(|w| matches(w, s)) || s.frame >= self.frame + GIVE_UP)
    }
}

/// The frames whose raw data to keep, shared with the PiSP pipeline's copy hook.
pub(crate) type Targets = Arc<Mutex<Vec<Target>>>;

/// Exposure time and analogue gain for a total exposure (seconds × gain), keeping the gain
/// AE uses where the exposure time allows, within the camera's limits at its frame rate.
pub(crate) fn split_exposure(total: f64, gain: f64, cam: &CameraConfig) -> (Duration, f64) {
    let min_t = cam.exposure_limits.0.as_secs_f64().max(1e-6);
    let frame = cam
        .frame_duration_limits
        .1
        .saturating_sub(cam.exposure_margin);
    let max_t = cam
        .exposure_limits
        .1
        .as_secs_f64()
        .min(frame.as_secs_f64())
        .max(min_t);
    let (gmin, gmax) = cam.analogue_gain_limits;
    let gmax = gmax.max(gmin);
    let g0 = gain.clamp(gmin, gmax);
    let mut t = (total / g0).clamp(min_t, max_t);
    let g = (total / t).clamp(gmin, gmax);
    if g <= gmin {
        t = (total / g).clamp(min_t, max_t);
    }
    (Duration::from_secs_f64(t), g)
}

#[derive(Debug)]
enum Phase {
    /// Controls not handed over yet.
    Pending,
    /// Controls handed over; waiting for the request they make.
    Requested,
    /// Waiting for frame `target`.
    Armed(Target),
    /// The raw frame is held.
    Held(Box<HeldRaw>),
}

struct Shot {
    ev: f64,
    want: Option<(Duration, f64)>,
    /// The frame its exposure lands on.
    target: Option<u64>,
    phase: Phase,
}

struct Active {
    job: StillJob,
    shots: Vec<Shot>,
    /// The application's controls to hand back.
    saved: Option<Controls>,
}

/// See the [module documentation](self).
pub(crate) struct StillRunner {
    loop_controls: Arc<LoopControls>,
    queue: Vec<StillJob>,
    active: Option<Active>,
    targets: Targets,
    processor: Option<StillProcessor>,
    spawn: Box<dyn FnMut() -> StillProcessor + Send>,
    /// The latest frame: what produced it, AE's total exposure, AE locked.
    last: Option<(SensorValues, f64, bool)>,
}

impl StillRunner {
    pub(crate) fn new(
        loop_controls: Arc<LoopControls>,
        spawn: Box<dyn FnMut() -> StillProcessor + Send>,
    ) -> Self {
        Self {
            loop_controls,
            queue: Vec::new(),
            active: None,
            targets: Arc::new(Mutex::new(Vec::new())),
            processor: None,
            spawn,
            last: None,
        }
    }

    /// The frames to copy (for the PiSP pipeline's copy hook).
    pub(crate) fn targets(&self) -> Targets {
        Arc::clone(&self.targets)
    }

    /// Whether frame `s`'s raw data is wanted.
    pub(crate) fn wants(&self, s: &SensorValues) -> bool {
        self.targets.lock().iter().any(|t| t.wants(s))
    }

    fn sync_targets(&self) {
        let mut t = self.targets.lock();
        t.clear();
        if let Some(a) = &self.active {
            t.extend(a.shots.iter().filter_map(|s| match s.phase {
                Phase::Armed(t) => Some(t),
                _ => None,
            }));
        }
    }

    fn fail(&mut self, host: &mut dyn StillHost, e: CaptureError) {
        if let Some(a) = self.active.take() {
            if let Some(c) = a.saved {
                host.set_controls(c);
            }
            let _ = a.job.reply.send(Err(e));
        }
        self.sync_targets();
    }

    /// Before the worker asks for the next frame: picks up new requests and starts one.
    pub(crate) fn before_frame(&mut self, host: &mut dyn StillHost) {
        self.queue.extend(self.loop_controls.take_stills());
        // A request starts once a frame has said what AE is doing.
        if self.active.is_none() && !self.queue.is_empty() && self.last.is_some() {
            let job = self.queue.remove(0);
            self.start(host, job);
        }
        let timed_out = self
            .active
            .as_ref()
            .is_some_and(|a| a.job.requested.elapsed() > a.job.request.timeout);
        if timed_out {
            self.fail(host, CaptureError::Backend("still: timed out".into()));
        }
    }

    fn start(&mut self, host: &mut dyn StillHost, job: StillJob) {
        let cam = host.camera();
        let (sensor, total, _) = self.last.unwrap_or((
            SensorValues {
                frame: 0,
                exposure: Duration::from_millis(10),
                analogue_gain: 1.0,
                digital_gain: 1.0,
                frame_duration: Duration::from_millis(33),
                verified: false,
            },
            0.0,
            false,
        ));
        let base = if total > 0.0 {
            total
        } else {
            sensor.exposure.as_secs_f64() * sensor.analogue_gain
        };
        let shots: Vec<Shot> = match &job.request.exposure {
            StillExposure::Current => vec![Shot {
                ev: 0.0,
                want: None,
                target: None,
                phase: Phase::Pending,
            }],
            StillExposure::Fixed { exposure, gain } => vec![Shot {
                ev: 0.0,
                want: Some((*exposure, *gain)),
                target: None,
                phase: Phase::Pending,
            }],
            StillExposure::Bracket(evs) => evs
                .iter()
                .map(|&ev| Shot {
                    ev,
                    want: Some(split_exposure(
                        base * 2f64.powf(ev),
                        sensor.analogue_gain,
                        &cam,
                    )),
                    target: None,
                    phase: Phase::Pending,
                })
                .collect(),
        };
        if shots.is_empty() {
            let _ = job.reply.send(Ok(StillCapture::default()));
            return;
        }
        let fixed = shots.iter().any(|s| s.want.is_some());
        self.active = Some(Active {
            job,
            shots,
            saved: fixed.then(|| self.loop_controls.current()),
        });
        self.advance(host, None);
    }

    /// Hands the next pending shot's controls to the loop, or arms a shot at AE's exposure.
    fn advance(&mut self, host: &mut dyn StillHost, next_frame: Option<u64>) {
        let ae_locked = self.last.is_some_and(|l| l.2);
        let Some(a) = self.active.as_mut() else {
            return;
        };
        if a.shots.iter().any(|s| matches!(s.phase, Phase::Requested)) {
            return;
        }
        let settle = a.job.request.settle;
        if let Some(shot) = a
            .shots
            .iter_mut()
            .find(|s| matches!(s.phase, Phase::Pending))
        {
            match shot.want {
                Some((exposure, gain)) => {
                    let base = a.saved.clone().unwrap_or_default();
                    host.set_controls(Controls {
                        ae_enable: true,
                        exposure: Some(exposure),
                        analogue_gain: Some(gain),
                        ..base
                    });
                    shot.phase = Phase::Requested;
                }
                None if !settle || ae_locked => {
                    shot.phase = Phase::Armed(Target {
                        frame: next_frame.unwrap_or(0),
                        want: None,
                    });
                }
                None => {}
            }
        } else if let Some(c) = a.saved.take() {
            // Every exposure is on its way: AE takes over again from the next request.
            host.set_controls(c);
        }
        self.sync_targets();
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
        self.last = Some((*s, total, ae_locked));
        let Some(a) = self.active.as_mut() else {
            return;
        };
        if let Some(raw) = raw
            && let Some(shot) = a.shots.iter_mut().find(|sh| match sh.phase {
                Phase::Armed(t) => t.wants(s),
                _ => false,
            })
        {
            shot.phase = Phase::Held(raw);
        }
        let requested = a
            .shots
            .iter_mut()
            .find(|sh| matches!(sh.phase, Phase::Requested));
        if let (Some(shot), Some(l), Some(r)) = (requested, lands, request) {
            let frame = l.max(s.frame + 1);
            shot.target = Some(frame);
            shot.phase = Phase::Armed(Target {
                frame,
                want: Some((r.exposure, r.analogue_gain)),
            });
        }
        self.advance(host, Some(s.frame + 1));
        let done = self
            .active
            .as_ref()
            .is_some_and(|a| a.shots.iter().all(|s| matches!(s.phase, Phase::Held(_))));
        if done && let Some(a) = self.active.take() {
            let shots = a
                .shots
                .into_iter()
                .filter_map(|s| match s.phase {
                    Phase::Held(raw) => Some(HeldShot {
                        ev: s.ev,
                        want: s.want,
                        target: s.target,
                        raw,
                    }),
                    _ => None,
                })
                .collect();
            let p = self.processor.get_or_insert_with(&mut self.spawn);
            p.submit(Batch { job: a.job, shots });
            self.sync_targets();
        }
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn exposures_split_within_the_frame() {
        let c = cam();
        // 10 ms at gain 2: more keeps the gain until the frame is full.
        let (t, g) = split_exposure(0.020, 2.0, &c);
        assert_eq!((t, g), (Duration::from_millis(10), 2.0));
        let (t, g) = split_exposure(0.080, 2.0, &c);
        assert_eq!(t, Duration::from_micros(32_500));
        assert!((g - 0.080 / 0.0325).abs() < 1e-9);
        // Less: shorter exposure at the same gain; below gain 1 the exposure gives.
        let (t, g) = split_exposure(0.002, 4.0, &c);
        assert_eq!((t, g), (Duration::from_micros(500), 4.0));
        let (t, g) = split_exposure(0.0001, 0.5, &c);
        assert_eq!((t, g), (Duration::from_micros(100), 1.0));
    }

    #[test]
    fn targets_wait_for_their_exposure_then_give_up() {
        let t = Target {
            frame: 10,
            want: Some((Duration::from_millis(10), 2.0)),
        };
        let s = |frame, ms: u64, g| SensorValues {
            frame,
            exposure: Duration::from_millis(ms),
            analogue_gain: g,
            digital_gain: 1.0,
            frame_duration: Duration::from_millis(33),
            verified: true,
        };
        assert!(!t.wants(&s(9, 10, 2.0)));
        assert!(t.wants(&s(10, 10, 2.0)));
        assert!(!t.wants(&s(11, 20, 2.0)));
        assert!(t.wants(&s(14, 20, 2.0)));
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
