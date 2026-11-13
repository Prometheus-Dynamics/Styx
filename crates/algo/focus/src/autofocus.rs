//! Autofocus over a Lemnos lens: styx-aaa's AF decides where the lens goes, the lens device moves
//! it, and the position comes back as the normalised Focus control.
//!
//! Per frame the caller gives the frame's [`Statistics`] and its [`FrameMetadata`]. The loop
//! fills in where the lens was for the frame (the moves written so far, and whether the lens had
//! settled by then), runs AF, sends any new [`LensRequest`] to the device, and reports the Focus
//! value of the lens position for the frame.
//!
//! * A move the chip cannot take yet (`AGAIN`: it powers up on its first move) is kept and asked
//!   again on the next frame. Other failures keep the move too, retried with a backoff that starts
//!   at 100 ms, doubles, and is capped at 5 s. The fault and backoff are visible through
//!   [`Autofocus::fault`]. Nothing gives up and nothing switches the lens off.
//! * The lens control writes AF's moves and nothing else; the Lemnos driver paces the chip.

use core::time::Duration;
use std::collections::VecDeque;
use std::time::Instant;

use lemnos_core::Status;
use styx_aaa::algos::af::{Af, AfTuning};
use styx_aaa::{
    AfStatus, Algorithm, CameraConfig, FrameMetadata as AafMetadata, LensConfig, LensRequest,
    LensState, Params, Statistics,
};
use styx_controls::{Control, ControlValue, FrameMetadata};
use styx_lemnos::Client;

use crate::lens::{Direction, Error, FocusScale, LensDevice, resolve_direction};
use lemnos_contract_lens::FocusDirection;

/// What the loop is configured with.
#[derive(Debug, Clone, PartialEq)]
pub struct AutofocusConfig {
    /// AF's tuning: ranges, speeds, and the dioptre map.
    pub tuning: AfTuning,
    /// Frames from a move's request to the frame it is for (the lens control's delay).
    pub delay_frames: u32,
    /// Time for a move to settle. `lens/1` reports none, so it is configured; the lens counts as
    /// settled from the frame that is this long after a move.
    pub settle: Duration,
    /// The frame duration the settle time is counted in.
    pub frame_duration: Duration,
}

impl Default for AutofocusConfig {
    fn default() -> Self {
        Self {
            tuning: AfTuning::default(),
            delay_frames: 1,
            settle: Duration::from_millis(12),
            frame_duration: Duration::from_millis(33),
        }
    }
}

/// One frame's result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Focus {
    /// AF's status for the frame.
    pub status: AfStatus,
    /// The lens position for the frame, normalised (0 nearest, 1000 farthest). `None` before the
    /// lens has a position.
    pub value: Option<u32>,
    /// A move AF asked for this frame, if it is news.
    pub request: Option<LensRequest>,
}

impl Focus {
    /// Writes the Focus value of this frame into the frame's metadata (when there is one).
    pub fn stamp(&self, meta: &mut FrameMetadata) {
        if let Some(v) = self.value {
            meta.controls[Control::Focus.index()] = ControlValue::Manual(v);
        }
    }
}

/// A move on the lens's schedule: the code it went to, the first frame exposed with the lens there,
/// and the frames it takes to settle.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Move {
    frame: u64,
    position: u16,
    settle_frames: u64,
}

/// A move AF asked for that the device has not taken yet.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pending {
    position: u16,
    /// The frame AF asked for it for.
    frame: u64,
}

const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// Autofocus for one lens. See the [module documentation](self).
#[derive(Debug)]
pub struct Autofocus {
    lens: LensDevice,
    scale: FocusScale,
    af: Af,
    params: Params,
    /// The configured settle time in frames, for a move the lens cannot say a settle for.
    settle_fallback: u64,
    /// The frame duration the settle times are counted in.
    frame: Duration,
    /// Where the device last took a move to (`None` before its first).
    last_taken: Option<u16>,
    /// Moves the device has taken, oldest first. The front is the one in force once no later move
    /// is due.
    schedule: VecDeque<Move>,
    /// The last request AF made, to tell news from a repeat.
    last_request: Option<LensRequest>,
    /// The move the device has not taken yet. AF does not run while there is one: its measurements
    /// would be of a lens that is not where AF thinks it is.
    pending: Option<Pending>,
    /// AF's status as of the last frame it ran.
    status: AfStatus,
    /// Retry state for `pending`: the status that refused it, the backoff in force, the next
    /// delay to use, and when to retry.
    fault: Option<Status>,
    waited: Duration,
    delay: Duration,
    retry_at: Option<Instant>,
    /// Moves the device has taken.
    taken: u32,
}

impl Autofocus {
    /// Opens the lens device `name` on `client`, sets AF up for the lens's range, and writes
    /// AF's start-up move. A chip that is still powering up takes it on a later frame.
    pub fn open(client: &Client, name: &str, config: AutofocusConfig) -> Result<Self, Error> {
        let mut lens = LensDevice::open(client, name)?;
        let range = lens.range();
        let lens_config = LensConfig {
            range: (i32::from(range.min), i32::from(range.max)),
            delay: config.delay_frames,
            map: None,
        };
        let map = Af::lens_map(&config.tuning, &lens_config);
        // The lens's own direction when it has one; the AF map's otherwise (never a guess).
        let answer = match lens.direction() {
            Ok(answer) => answer,
            Err(Status::NOT_SUPPORTED) => FocusDirection::Unknown,
            Err(status) => return Err(Error::Call(status)),
        };
        let scale = FocusScale::new(range, resolve_direction(answer, Direction::of_map(&map)))?;
        let mut af = Af::new(config.tuning)?;
        let camera = CameraConfig {
            lens: Some(lens_config),
            ..CameraConfig::default()
        };
        af.prepare(&camera)?;
        let mut params = Params::default();
        af.initial(&mut params);
        let mut this = Self {
            lens,
            scale,
            af,
            params,
            settle_fallback: settle_frames(config.settle, config.frame_duration),
            frame: config.frame_duration,
            last_taken: None,
            schedule: VecDeque::new(),
            last_request: None,
            pending: None,
            status: AfStatus::default(),
            fault: None,
            waited: Duration::ZERO,
            delay: BACKOFF_START,
            retry_at: None,
            taken: 0,
        };
        if let Some(request) = this.params.lens {
            this.note(request);
        }
        this.flush(0, Instant::now());
        Ok(this)
    }

    /// Runs AF on one frame and moves the lens as it asks. `meta.lens` is filled in from the moves
    /// the device has taken; the frame's exposure is what `stats` measured.
    ///
    /// While a move is not taken, AF does not run on the frame: the frame's statistics are
    /// dropped, and AF resumes on the first frame after the move lands.
    pub fn process(&mut self, stats: &Statistics, meta: &mut AafMetadata) -> Focus {
        let frame = meta.frame;
        let now = Instant::now();
        self.flush(frame, now);
        let mut request = None;
        if self.pending.is_none() {
            meta.lens = self.lens_state(frame);
            self.af.process(stats, meta, &mut self.params);
            self.status = self.params.af;
            if let Some(r) = self.params.lens.filter(|r| self.last_request != Some(*r)) {
                self.note(r);
                self.flush(frame, now);
                request = Some(r);
            }
        }
        Focus {
            status: self.status,
            value: self.lens_state(frame).map(|l| self.scale.value(l.position)),
            request,
        }
    }

    /// The Focus value of the lens position in force for `frame` (`None` before the lens has one).
    pub fn value_for(&mut self, frame: u64) -> Option<u32> {
        self.lens_state(frame).map(|l| self.scale.value(l.position))
    }

    /// The lens device's range and Focus mapping.
    pub fn scale(&self) -> FocusScale {
        self.scale
    }

    /// Frames the settle time of the last move the device took spans: the lens's own figure for
    /// that move when it has one, the configured settle time when it has not. `None` before the
    /// device has taken a move.
    pub fn last_settle_frames(&self) -> Option<u64> {
        self.schedule.back().map(|m| m.settle_frames)
    }

    /// Moves the device has taken since the loop started.
    pub fn moves_taken(&self) -> u32 {
        self.taken
    }

    /// The move the device has not taken yet, if any.
    pub fn pending(&self) -> Option<u16> {
        self.pending.map(|p| p.position)
    }

    /// The status that refused the pending move, and the backoff in force before it is retried
    /// (`None` while the chip is only powering up, or when nothing is pending).
    pub fn fault(&self) -> Option<(Status, Duration)> {
        self.fault.map(|s| (s, self.waited))
    }

    /// Reads the lens's own position (the last one it took), through the device.
    pub fn device_position(&mut self) -> Result<Option<u16>, Status> {
        self.lens.position()
    }

    /// Records AF's move as the one to write now. It joins the schedule when the device takes it.
    fn note(&mut self, request: LensRequest) {
        self.last_request = Some(request);
        let range = self.lens.range();
        let position = match u16::try_from(request.position) {
            Ok(p) => p.clamp(range.min, range.max),
            Err(_) => range.max,
        };
        self.pending = Some(Pending {
            position,
            frame: request.frame,
        });
    }

    /// Writes the pending move if the device can take it now. A move taken on `frame` or later
    /// is in force from the frame it was asked for, or from `frame` when it was asked for earlier.
    fn flush(&mut self, frame: u64, now: Instant) {
        let Some(pending) = self.pending else {
            return;
        };
        if self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        match self.lens.set_position(pending.position) {
            Ok(true) => {
                self.pending = None;
                self.taken += 1;
                self.fault = None;
                self.waited = Duration::ZERO;
                self.delay = BACKOFF_START;
                self.retry_at = None;
                let settle_frames = self.settle_for(pending.position);
                self.schedule.push_back(Move {
                    frame: pending.frame.max(frame),
                    position: pending.position,
                    settle_frames,
                });
            }
            // Powering up: the same move, on the next frame.
            Ok(false) => {}
            Err(status) => {
                self.fault = Some(status);
                self.waited = self.delay;
                self.retry_at = Some(now + self.delay);
                self.delay = (self.delay * 2).min(BACKOFF_CAP);
            }
        }
    }

    /// Frames the lens takes to settle after a move to `position`: the lens's own figure for the
    /// move's distance, from the last move the device took (or the near end before the first), or
    /// the configured settle time when the lens has none.
    fn settle_for(&mut self, position: u16) -> u64 {
        let from = self.last_taken.unwrap_or(self.lens.range().min);
        self.last_taken = Some(position);
        let distance = position.abs_diff(from);
        match self.lens.settle_us(distance) {
            Some(us) => settle_frames_of_us(us, self.frame),
            None => self.settle_fallback,
        }
    }

    /// Where the lens was for `frame`: the last move due by then, and whether it had settled.
    fn lens_state(&mut self, frame: u64) -> Option<LensState> {
        while self.schedule.len() > 1 && self.schedule[1].frame <= frame {
            self.schedule.pop_front();
        }
        let m = *self.schedule.front()?;
        if m.frame > frame {
            return None;
        }
        Some(LensState {
            position: f64::from(m.position),
            settled: frame >= m.frame + m.settle_frames,
        })
    }
}

/// Frames a settle time spans, rounded up (at least one).
fn settle_frames(settle: Duration, frame: Duration) -> u64 {
    let frame_ns = frame.as_nanos().max(1);
    u64::try_from(settle.as_nanos().div_ceil(frame_ns))
        .unwrap_or(u64::MAX)
        .max(1)
}

/// Frames a settle time in microseconds spans, rounded up (at least one).
fn settle_frames_of_us(us: u32, frame: Duration) -> u64 {
    settle_frames(Duration::from_micros(u64::from(us)), frame)
}
