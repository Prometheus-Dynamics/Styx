//! Playback controls of a replay capture: play and pause, speed, loop, seek, step one frame,
//! and the position, duration and state, as controls (so a camera service serving a replay
//! offers them to its clients like a live camera's).
//!
//! Times are in milliseconds from the recording's first frame (`Uint`); frames count from 0
//! within a pass. The duration and frame count are found by a scan of the recording when the
//! replay starts (0 until it ends).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use styx_capture::prelude::*;

use super::CaptureError;

/// The control ids.
pub mod ids {
    use styx_capture::prelude::ControlId;

    /// Playing (`true`) or paused (`Bool`).
    pub const REPLAY_PLAYING: ControlId = ControlId(0xF500_0001);
    /// Speed (`Float`): 0.25 to 4 times the recorded rate; 0: every frame as fast as the
    /// consumers take them.
    pub const REPLAY_SPEED: ControlId = ControlId(0xF500_0002);
    /// Start again from the first frame at the end (`Bool`).
    pub const REPLAY_LOOP: ControlId = ControlId(0xF500_0003);
    /// Go to this time (`Uint` ms from the first frame; write only): the first frame at or
    /// after it is delivered next (also while paused).
    pub const REPLAY_SEEK_MS: ControlId = ControlId(0xF500_0004);
    /// Go to this frame (`Uint`, from 0; write only), as [`REPLAY_SEEK_MS`].
    pub const REPLAY_SEEK_FRAME: ControlId = ControlId(0xF500_0005);
    /// Deliver the next frame, then pause (`Bool`, write only; any value).
    pub const REPLAY_STEP: ControlId = ControlId(0xF500_0006);
    /// The time of the last frame delivered (`Uint` ms from the first frame; read only).
    pub const REPLAY_POSITION_MS: ControlId = ControlId(0xF500_0007);
    /// The last frame delivered (`Uint`, from 0; read only).
    pub const REPLAY_FRAME: ControlId = ControlId(0xF500_0008);
    /// The recording's length (`Uint` ms, first to last frame; read only; 0 until known).
    pub const REPLAY_DURATION_MS: ControlId = ControlId(0xF500_0009);
    /// The recording's frames (`Uint`; read only; 0 until known).
    pub const REPLAY_FRAME_COUNT: ControlId = ControlId(0xF500_000A);
    /// Playing, paused or ended (`Int` menu; read only).
    pub const REPLAY_STATE: ControlId = ControlId(0xF500_000B);
}

const STATES: [&str; 3] = ["playing", "paused", "ended"];

/// The playback controls' descriptors.
pub(crate) fn metas() -> Vec<ControlMeta> {
    let meta = |id, name: &str, kind, min, max, default, access| ControlMeta {
        id,
        name: name.into(),
        kind,
        access,
        min,
        max,
        default,
        step: None,
        menu: None,
        metadata: ControlMetadata::default(),
    };
    let (rw, ro) = (Access::ReadWrite, Access::ReadOnly);
    let ms = |v| ControlValue::Uint(v);
    vec![
        meta(
            ids::REPLAY_PLAYING,
            "replay_playing",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
            rw,
        ),
        meta(
            ids::REPLAY_SPEED,
            "replay_speed",
            ControlKind::Float,
            ControlValue::Float(0.0),
            ControlValue::Float(4.0),
            ControlValue::Float(1.0),
            rw,
        ),
        meta(
            ids::REPLAY_LOOP,
            "replay_loop",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(false),
            rw,
        ),
        meta(
            ids::REPLAY_SEEK_MS,
            "replay_seek_ms",
            ControlKind::Uint,
            ms(0),
            ms(u32::MAX),
            ms(0),
            rw,
        ),
        meta(
            ids::REPLAY_SEEK_FRAME,
            "replay_seek_frame",
            ControlKind::Uint,
            ms(0),
            ms(u32::MAX),
            ms(0),
            rw,
        ),
        meta(
            ids::REPLAY_STEP,
            "replay_step",
            ControlKind::Bool,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Bool(true),
            rw,
        ),
        meta(
            ids::REPLAY_POSITION_MS,
            "replay_position_ms",
            ControlKind::Uint,
            ms(0),
            ms(u32::MAX),
            ms(0),
            ro,
        ),
        meta(
            ids::REPLAY_FRAME,
            "replay_frame",
            ControlKind::Uint,
            ms(0),
            ms(u32::MAX),
            ms(0),
            ro,
        ),
        meta(
            ids::REPLAY_DURATION_MS,
            "replay_duration_ms",
            ControlKind::Uint,
            ms(0),
            ms(u32::MAX),
            ms(0),
            ro,
        ),
        meta(
            ids::REPLAY_FRAME_COUNT,
            "replay_frame_count",
            ControlKind::Uint,
            ms(0),
            ms(u32::MAX),
            ms(0),
            ro,
        ),
        ControlMeta {
            menu: Some(MenuEntry::names(STATES.iter().copied())),
            ..meta(
                ids::REPLAY_STATE,
                "replay_state",
                ControlKind::Int,
                ControlValue::Int(0),
                ControlValue::Int(2),
                ControlValue::Int(0),
                ro,
            )
        },
    ]
}

/// Where to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seek {
    /// The first frame at or after this many nanoseconds from the first frame.
    Time(u64),
    Frame(u64),
}

/// What the worker is asked to do (changed by the controls, taken by the worker).
#[derive(Debug)]
pub(crate) struct Requests {
    pub(crate) playing: bool,
    /// 0: unpaced.
    pub(crate) speed: f32,
    pub(crate) looping: bool,
    pub(crate) seek: Option<Seek>,
    pub(crate) steps: u32,
    pub(crate) stopped: bool,
    /// Bumped on every change, so the worker can tell a change from a spurious wake.
    pub(crate) generation: u64,
}

/// A replay capture's playback state, shared by its worker and its control plane.
#[derive(Debug)]
pub struct ReplayControls {
    pub(crate) requests: Mutex<Requests>,
    pub(crate) changed: Condvar,
    position_ns: AtomicU64,
    frame: AtomicU64,
    duration_ns: AtomicU64,
    frame_count: AtomicU64,
    /// 0 playing, 1 paused, 2 ended.
    state: AtomicU32,
}

impl ReplayControls {
    pub(crate) fn new(speed: f32, looping: bool) -> Self {
        Self {
            requests: Mutex::new(Requests {
                playing: true,
                speed,
                looping,
                seek: None,
                steps: 0,
                stopped: false,
                generation: 0,
            }),
            changed: Condvar::new(),
            position_ns: AtomicU64::new(0),
            frame: AtomicU64::new(0),
            duration_ns: AtomicU64::new(0),
            frame_count: AtomicU64::new(0),
            state: AtomicU32::new(0),
        }
    }

    fn change(&self, f: impl FnOnce(&mut Requests)) {
        let mut r = self.requests.lock();
        f(&mut r);
        r.generation += 1;
        drop(r);
        self.changed.notify_all();
    }

    pub(crate) fn apply(&self, id: ControlId, value: ControlValue) -> Result<(), CaptureError> {
        let number = match value {
            ControlValue::Bool(b) => Some(f64::from(u8::from(b))),
            ControlValue::Int(v) => Some(f64::from(v)),
            ControlValue::Uint(v) => Some(f64::from(v)),
            ControlValue::Float(v) => Some(f64::from(v)),
            _ => None,
        }
        .ok_or_else(|| CaptureError::control_apply(format!("replay control: {value:?}")))?;
        match id {
            ids::REPLAY_PLAYING => self.change(|r| r.playing = number != 0.0),
            ids::REPLAY_SPEED => {
                let speed = number as f32;
                if !(speed == 0.0 || (0.25..=4.0).contains(&speed)) {
                    return Err(CaptureError::control_apply(
                        "replay speed: 0.25 to 4, or 0 for as fast as the consumers",
                    ));
                }
                self.change(|r| r.speed = speed);
            }
            ids::REPLAY_LOOP => self.change(|r| r.looping = number != 0.0),
            ids::REPLAY_SEEK_MS => {
                self.change(|r| r.seek = Some(Seek::Time(number.max(0.0) as u64 * 1_000_000)))
            }
            ids::REPLAY_SEEK_FRAME => {
                self.change(|r| r.seek = Some(Seek::Frame(number.max(0.0) as u64)))
            }
            ids::REPLAY_STEP => self.change(|r| {
                r.steps += 1;
                r.playing = false;
            }),
            ids::REPLAY_POSITION_MS
            | ids::REPLAY_FRAME
            | ids::REPLAY_DURATION_MS
            | ids::REPLAY_FRAME_COUNT
            | ids::REPLAY_STATE => {
                return Err(CaptureError::control_apply("replay position is read only"));
            }
            _ => return Err(CaptureError::ControlUnsupported),
        }
        Ok(())
    }

    pub(crate) fn read(&self, id: ControlId) -> Result<ControlValue, CaptureError> {
        let ms = |a: &AtomicU64| {
            ControlValue::Uint(
                u32::try_from(a.load(Ordering::Acquire) / 1_000_000).unwrap_or(u32::MAX),
            )
        };
        let count = |a: &AtomicU64| {
            ControlValue::Uint(u32::try_from(a.load(Ordering::Acquire)).unwrap_or(u32::MAX))
        };
        let r = self.requests.lock();
        Ok(match id {
            ids::REPLAY_PLAYING => ControlValue::Bool(r.playing),
            ids::REPLAY_SPEED => ControlValue::Float(r.speed),
            ids::REPLAY_LOOP => ControlValue::Bool(r.looping),
            ids::REPLAY_POSITION_MS | ids::REPLAY_SEEK_MS => ms(&self.position_ns),
            ids::REPLAY_FRAME | ids::REPLAY_SEEK_FRAME => count(&self.frame),
            ids::REPLAY_DURATION_MS => ms(&self.duration_ns),
            ids::REPLAY_FRAME_COUNT => count(&self.frame_count),
            ids::REPLAY_STATE => ControlValue::Int(self.state.load(Ordering::Acquire) as i32),
            ids::REPLAY_STEP => ControlValue::Bool(false),
            _ => return Err(CaptureError::ControlUnsupported),
        })
    }

    /// The worker delivered frame `frame` at `position` from the first frame.
    pub(crate) fn delivered(&self, frame: u64, position: Duration) {
        self.frame.store(frame, Ordering::Release);
        self.position_ns
            .store(position.as_nanos() as u64, Ordering::Release);
    }

    /// The recording's length and frames, once scanned.
    pub(crate) fn set_length(&self, duration: Duration, frames: u64) {
        self.duration_ns
            .store(duration.as_nanos() as u64, Ordering::Release);
        self.frame_count.store(frames, Ordering::Release);
    }

    /// 0 playing, 1 paused, 2 ended.
    pub(crate) fn set_state(&self, state: u32) {
        self.state.store(state, Ordering::Release);
    }
}
