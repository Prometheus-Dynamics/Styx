//! Still and bracket decisions, platform-neutral (`no_std` + `alloc`): which frames' raw data a
//! request keeps (the next one, the first after AE locks, or the frame a fixed or bracketed
//! exposure lands on, as the control schedule says), driving the 3A loop's controls for fixed
//! and bracketed exposures on the control schedule and handing them back to AE afterwards,
//! giving up on an exposure that does not come, timing requests out, and whether a held frame
//! landed.
//!
//! What a platform keeps is the mechanics: holding a frame's raw data ([`crate::still::HeldRaw`]),
//! reprocessing it (the PiSP back end's node group 1, or [`crate::still::soft_still`] on a
//! thread), encoding (JPEG, DNG) and answering the request. The loop calls
//! [`StillRunner::before_frame`] and [`StillRunner::after_frame`] around each frame; a request
//! ends in a [`StillOutcome`].
//!
//! ```text
//! submit ─► queue ─► start (first frame that said what AE does) ─► per shot:
//!   AE's exposure:   armed on the next frame (after AE locks with `settle`)
//!   fixed / bracket: controls handed to the loop ─► the request they make names its landing
//!                    frame ─► armed for that frame and that exposure (or what comes GIVE_UP
//!                    frames later) ─► the next shot's controls ... ─► AE's controls back
//! every shot held ─► StillOutcome::Taken
//! ```

#[cfg(not(feature = "std"))]
use crate::math::Float as _;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::time::Duration;

use styx_algo::{CameraConfig, Controls, SensorRequest};

use crate::controller::SensorValues;
use crate::soft::SoftLoop;

/// Frames after its target a shot waits for its exposure before taking what comes.
pub const GIVE_UP: u64 = 4;

/// Whether frame `s` was produced with the exposure and gain `want` (within 3%, or 60 µs).
pub fn matches(want: (Duration, f64), s: &SensorValues) -> bool {
    let (t, g) = (want.0.as_secs_f64(), want.1);
    let e = s.exposure.as_secs_f64();
    (e - t).abs() <= (0.03 * t).max(60e-6) && (s.analogue_gain - g).abs() <= 0.03 * g
}

/// A frame the stream wants the raw data of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    /// From this frame on.
    pub frame: u64,
    /// With this exposure and gain (or any frame after `frame + GIVE_UP`).
    pub want: Option<(Duration, f64)>,
}

impl Target {
    /// Whether frame `s` is this target's.
    pub fn wants(&self, s: &SensorValues) -> bool {
        s.frame >= self.frame
            && (self.want.is_none_or(|w| matches(w, s)) || s.frame >= self.frame + GIVE_UP)
    }
}

/// Exposure time and analogue gain for a total exposure (seconds × gain), keeping the gain
/// AE uses where the exposure time allows, within the camera's limits at its frame rate.
pub fn split_exposure(total: f64, gain: f64, cam: &CameraConfig) -> (Duration, f64) {
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

/// The ISP digital gain a fixed or bracketed shot is processed with: the white balance's green
/// gain (`green`), and what the sensor fell short of the exposure asked for. (The stream's
/// digital gain makes up for AE's exposure not having landed yet; in a bracket AE already aims
/// at the next shot, so a shot with its own exposure does not take it.)
pub fn fixed_exposure_gain(want: (Duration, f64), got: &SensorValues, green: f64) -> f64 {
    let asked = want.0.as_secs_f64() * want.1;
    let got = got.total_exposure().max(1e-12);
    green.max(1e-6) * (asked / got).max(1.0)
}

/// What exposure a still request takes.
#[derive(Clone, Debug, PartialEq)]
pub enum ShotExposure {
    /// AE's, on the next frame (or the first after AE locks, see [`StillOrder::settle`]).
    Current,
    /// A fixed exposure and analogue gain.
    Fixed {
        /// Exposure time.
        exposure: Duration,
        /// Analogue gain.
        gain: f64,
    },
    /// One shot per value, at AE's total exposure times `2^ev`, on consecutive frames where
    /// the sensor's control delays allow.
    Bracket(Vec<f64>),
}

/// A still request as the decisions see it.
#[derive(Clone, Debug, PartialEq)]
pub struct StillOrder {
    /// The exposure.
    pub exposure: ShotExposure,
    /// A shot at AE's exposure waits for AE to lock.
    pub settle: bool,
    /// The request fails when it is not taken within this long of `requested`.
    pub timeout: Duration,
    /// When it was made, on the clock [`StillRunner::before_frame`] is given.
    pub requested: Duration,
}

/// Where a still request's controls go: the 3A loop.
pub trait StillHost {
    /// Hands `c` to the loop's algorithms (from the next frame they run on).
    fn set_controls(&mut self, c: Controls);
    /// The application's controls, handed back after fixed exposures.
    fn controls(&mut self) -> Controls;
    /// The camera's limits (for bracket exposures).
    fn camera(&mut self) -> CameraConfig;
}

/// The software loop drives its own controller: the application's controls are the
/// controller's.
impl StillHost for SoftLoop {
    fn set_controls(&mut self, c: Controls) {
        self.controller().set_controls(c);
    }

    fn controls(&mut self) -> Controls {
        self.controller().controls().clone()
    }

    fn camera(&mut self) -> CameraConfig {
        self.info().camera.clone()
    }
}

/// One shot's held frame.
#[derive(Clone, Debug, PartialEq)]
pub struct HeldShot<H> {
    /// Its bracket value (0 for a single shot).
    pub ev: f64,
    /// The exposure and gain it asked for (`None`: AE's).
    pub want: Option<(Duration, f64)>,
    /// The frame its exposure was to land on.
    pub target: Option<u64>,
    /// The held frame (raw data and what produced it).
    pub raw: H,
}

impl<H> HeldShot<H> {
    /// Whether the shot is frame `sequence`, produced with `s`, on the frame its exposure was
    /// to land on and with that exposure.
    pub fn landed(&self, sequence: u64, s: &SensorValues) -> bool {
        self.target.is_none_or(|t| t == sequence) && self.want.is_none_or(|w| matches(w, s))
    }
}

/// Why a request failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StillFailure {
    /// Not taken within its timeout.
    TimedOut,
}

/// How a request ended.
#[derive(Debug)]
pub enum StillOutcome<J, H> {
    /// Every shot held, in the request's order (none for an empty bracket).
    Taken {
        /// The request.
        job: J,
        /// Its shots.
        shots: Vec<HeldShot<H>>,
    },
    /// It failed; the loop has AE's controls back.
    Failed {
        /// The request.
        job: J,
        /// Why.
        reason: StillFailure,
    },
}

/// What the loop made of a frame, for [`StillRunner::after_frame`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LoopReport {
    /// The frame the request made from its statistics lands on.
    pub lands: Option<u64>,
    /// That request.
    pub request: Option<SensorRequest>,
    /// AE's total exposure (seconds × gain; 0: not known).
    pub total_exposure: f64,
    /// AE locked.
    pub ae_locked: bool,
}

#[derive(Debug)]
enum Phase<H> {
    /// Controls not handed over yet.
    Pending,
    /// Controls handed over; waiting for the request they make.
    Requested,
    /// Waiting for frame `target`.
    Armed(Target),
    /// The raw frame is held.
    Held(H),
}

#[derive(Debug)]
struct Shot<H> {
    ev: f64,
    want: Option<(Duration, f64)>,
    target: Option<u64>,
    phase: Phase<H>,
}

#[derive(Debug)]
struct Active<J, H> {
    job: J,
    order: StillOrder,
    shots: Vec<Shot<H>>,
    /// The application's controls to hand back.
    saved: Option<Controls>,
}

/// See the [module documentation](self). `J` is the platform's request (its reply channel),
/// `H` a held frame.
#[derive(Debug)]
pub struct StillRunner<J, H> {
    queue: VecDeque<(J, StillOrder)>,
    active: Option<Active<J, H>>,
    targets: Vec<Target>,
    /// Bumped whenever `targets` changes.
    generation: u64,
    /// The latest frame: what produced it, AE's total exposure, AE locked.
    last: Option<(SensorValues, f64, bool)>,
}

impl<J, H> Default for StillRunner<J, H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<J, H> StillRunner<J, H> {
    /// No requests.
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            active: None,
            targets: Vec::new(),
            generation: 0,
            last: None,
        }
    }

    /// Queues a request; it starts once a frame has said what AE is doing.
    pub fn submit(&mut self, job: J, order: StillOrder) {
        self.queue.push_back((job, order));
    }

    /// Whether no request is queued or running.
    pub fn is_idle(&self) -> bool {
        self.active.is_none() && self.queue.is_empty()
    }

    /// The frames to keep the raw data of (for a copy hook running inside the ISP's frame).
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// Changes whenever [`Self::targets`] does.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether frame `s`'s raw data is wanted.
    pub fn wants(&self, s: &SensorValues) -> bool {
        self.targets.iter().any(|t| t.wants(s))
    }

    fn sync_targets(&mut self) {
        let before = self.targets.len();
        self.targets.clear();
        if let Some(a) = &self.active {
            self.targets
                .extend(a.shots.iter().filter_map(|s| match s.phase {
                    Phase::Armed(t) => Some(t),
                    _ => None,
                }));
        }
        if before != 0 || !self.targets.is_empty() {
            self.generation += 1;
        }
    }

    fn fail(
        &mut self,
        host: &mut impl StillHost,
        reason: StillFailure,
    ) -> Option<StillOutcome<J, H>> {
        let a = self.active.take()?;
        if let Some(c) = a.saved {
            host.set_controls(c);
        }
        self.sync_targets();
        Some(StillOutcome::Failed { job: a.job, reason })
    }

    /// Before the loop takes the next frame (`now` on the clock of
    /// [`StillOrder::requested`]): starts a queued request, times the running one out.
    pub fn before_frame(
        &mut self,
        host: &mut impl StillHost,
        now: Duration,
    ) -> Option<StillOutcome<J, H>> {
        if self.active.is_none()
            && self.last.is_some()
            && let Some((job, order)) = self.queue.pop_front()
            && let Some(done) = self.start(host, job, order)
        {
            return Some(done);
        }
        let timed_out = self
            .active
            .as_ref()
            .is_some_and(|a| now.saturating_sub(a.order.requested) > a.order.timeout);
        if timed_out {
            return self.fail(host, StillFailure::TimedOut);
        }
        None
    }

    fn start(
        &mut self,
        host: &mut impl StillHost,
        job: J,
        order: StillOrder,
    ) -> Option<StillOutcome<J, H>> {
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
        let shot = |ev, want| Shot {
            ev,
            want,
            target: None,
            phase: Phase::Pending,
        };
        let shots: Vec<Shot<H>> = match &order.exposure {
            ShotExposure::Current => alloc::vec![shot(0.0, None)],
            ShotExposure::Fixed { exposure, gain } => {
                alloc::vec![shot(0.0, Some((*exposure, *gain)))]
            }
            ShotExposure::Bracket(evs) => {
                let cam = host.camera();
                evs.iter()
                    .map(|&ev| {
                        let want = split_exposure(base * 2f64.powf(ev), sensor.analogue_gain, &cam);
                        shot(ev, Some(want))
                    })
                    .collect()
            }
        };
        if shots.is_empty() {
            return Some(StillOutcome::Taken {
                job,
                shots: Vec::new(),
            });
        }
        let fixed = shots.iter().any(|s| s.want.is_some());
        self.active = Some(Active {
            job,
            order,
            shots,
            saved: fixed.then(|| host.controls()),
        });
        self.advance(host, None);
        None
    }

    /// Hands the next pending shot's controls to the loop, or arms a shot at AE's exposure.
    fn advance(&mut self, host: &mut impl StillHost, next_frame: Option<u64>) {
        let ae_locked = self.last.is_some_and(|l| l.2);
        let Some(a) = self.active.as_mut() else {
            return;
        };
        if a.shots.iter().any(|s| matches!(s.phase, Phase::Requested)) {
            return;
        }
        let settle = a.order.settle;
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

    /// After frame `s`: what the loop made of it, and its raw data if [`Self::wants`] said so.
    pub fn after_frame(
        &mut self,
        host: &mut impl StillHost,
        s: &SensorValues,
        report: &LoopReport,
        raw: Option<H>,
    ) -> Option<StillOutcome<J, H>> {
        self.last = Some((*s, report.total_exposure, report.ae_locked));
        let a = self.active.as_mut()?;
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
        if let (Some(shot), Some(l), Some(r)) = (requested, report.lands, report.request) {
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
        if !done {
            return None;
        }
        let a = self.active.take()?;
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
        self.sync_targets();
        Some(StillOutcome::Taken { job: a.job, shots })
    }
}

#[cfg(test)]
#[path = "still_runner_tests.rs"]
mod tests;
