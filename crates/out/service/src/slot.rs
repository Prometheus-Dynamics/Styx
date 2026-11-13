//! One camera's shared state: what the worker thread reports and what the control socket reads.

use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use styx_controls::FrameMetadata;
use styx_controls::{Control, ControlValue, Update};
use styx_frame::FrameLease;
use styx_select::{Backend, Selected};

use crate::config::CameraConfig;

/// Where a camera's worker is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Listed by no backend right now: its worker waits for it to come back.
    Absent,
    /// Frames flow.
    Streaming,
    /// The backend is retrying on its own (its own message is in `last_error`).
    Retrying,
    /// Waiting before the next attempt. `next_delay` is the wait.
    Backoff,
    /// Stopped by a control request. Waits for `start`.
    Stopped,
    /// Turned off by the configured `auto_off_after` policy. Waits for `start`.
    Off,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            State::Absent => "absent",
            State::Streaming => "streaming",
            State::Retrying => "retrying",
            State::Backoff => "backoff",
            State::Stopped => "stopped",
            State::Off => "off",
        })
    }
}

/// The last frame a camera delivered: its identity and the metadata in force on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSummary {
    pub id: u64,
    pub timestamp_ns: u64,
    pub exposure_us: u32,
    pub gain_milli: u32,
    pub controls: [ControlValue; Control::COUNT],
}

/// What the service knows about one camera. Cloned out for the control socket.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraStatus {
    /// The camera's id (its best stable anchor, for example `lemnos:sensor0`).
    pub id: String,
    /// The backend that owns the camera now, if a scan has listed it.
    pub owner: Option<Backend>,
    /// Why that backend owns it, in one line (the chooser's reason).
    pub reason: Option<String>,
    pub state: State,
    /// Times the camera started streaming.
    pub starts: u64,
    /// Failures in a row (reset by a delivered frame, a start, or a manual `start`).
    pub failures: u32,
    /// Failures since the service started.
    pub total_failures: u64,
    /// The wait before the next attempt, while in `Backoff`.
    pub next_delay: Option<Duration>,
    /// The last failure's message, or the backend's last retry message.
    pub last_error: Option<String>,
    /// Frames delivered since the service started.
    pub frames: u64,
    /// Frame identities skipped (a gap in the stream), since the service started.
    pub skipped: u64,
    /// The last frame delivered.
    pub last_frame: Option<FrameSummary>,
}

/// One camera's slot: its settings, its current selection, its status and its command queue.
#[derive(Debug)]
pub struct Slot {
    pub id: String,
    pub settings: CameraConfig,
    /// The backend's listing for this camera, `None` while no scan lists it.
    pub selection: Mutex<Option<Selected>>,
    pub status: Mutex<CameraStatus>,
    pub commands: Sender<Command>,
    /// Set to end a backoff wait early (a control request, or shutdown).
    pub wake: AtomicBool,
}

/// A request to a camera's worker thread.
#[derive(Debug)]
pub enum Command {
    /// Start a stopped or turned-off camera again.
    Start,
    /// Stop the camera until `Start`.
    Stop,
    /// Post controls to the running camera. The reply is the backend's answer.
    Set {
        updates: Vec<Update>,
        reply: Sender<Result<(), String>>,
    },
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Slot {
    pub fn new(id: String, settings: CameraConfig, commands: Sender<Command>) -> Self {
        let status = CameraStatus {
            id: id.clone(),
            owner: None,
            reason: None,
            state: State::Absent,
            starts: 0,
            failures: 0,
            total_failures: 0,
            next_delay: None,
            last_error: None,
            frames: 0,
            skipped: 0,
            last_frame: None,
        };
        Self {
            id,
            settings,
            selection: Mutex::new(None),
            status: Mutex::new(status),
            commands,
            wake: AtomicBool::new(false),
        }
    }

    /// A copy of the camera's status.
    pub fn snapshot(&self) -> CameraStatus {
        lock(&self.status).clone()
    }

    /// The current selection.
    pub fn selected(&self) -> Option<Selected> {
        lock(&self.selection).clone()
    }

    /// Records a scan's answer: the owner and reason, or `None` when no scan lists it.
    pub fn set_selection(&self, selection: Option<&Selected>) {
        *lock(&self.selection) = selection.cloned();
        let mut status = lock(&self.status);
        match selection {
            Some(sel) => {
                status.owner = Some(sel.owner);
                status.reason = Some(sel.reason.to_string());
            }
            None => {
                status.reason = None;
                if status.state == State::Streaming {
                    status.state = State::Absent;
                }
            }
        }
    }

    pub(crate) fn set_state(&self, state: State) {
        let mut status = lock(&self.status);
        status.state = state;
        if state != State::Backoff {
            status.next_delay = None;
        }
    }

    pub(crate) fn started(&self) {
        let mut status = lock(&self.status);
        status.state = State::Streaming;
        status.starts += 1;
        status.next_delay = None;
    }

    pub(crate) fn retrying(&self, reason: String) {
        let mut status = lock(&self.status);
        status.state = State::Retrying;
        status.last_error = Some(reason);
    }

    pub(crate) fn resumed(&self) {
        let mut status = lock(&self.status);
        if status.state == State::Retrying {
            status.state = State::Streaming;
        }
    }

    pub(crate) fn failed(&self, reason: String, failures: u32) {
        let mut status = lock(&self.status);
        status.failures = failures;
        status.total_failures += 1;
        status.last_error = Some(reason);
    }

    pub(crate) fn backing_off(&self, delay: Duration) {
        let mut status = lock(&self.status);
        status.state = State::Backoff;
        status.next_delay = Some(delay);
    }

    pub(crate) fn reset_failures(&self) {
        lock(&self.status).failures = 0;
    }

    /// Called for every frame. Takes the status lock and writes numbers; nothing is allocated.
    pub(crate) fn frame(&self, lease: &FrameLease<FrameMetadata>) {
        let id = lease.id();
        let meta = lease.meta();
        let mut status = lock(&self.status);
        if let Some(last) = status.last_frame
            && id > last.id + 1
        {
            status.skipped += id - last.id - 1;
        }
        status.frames += 1;
        status.last_frame = Some(FrameSummary {
            id,
            timestamp_ns: lease.timestamp_ns(),
            exposure_us: meta.exposure_us,
            gain_milli: meta.analogue_gain_milli,
            controls: meta.controls,
        });
        if status.state != State::Streaming && status.state != State::Retrying {
            status.state = State::Streaming;
        }
    }

    /// The last frame's control values, for `controls`.
    pub fn current_controls(&self) -> Option<[ControlValue; Control::COUNT]> {
        lock(&self.status).last_frame.map(|f| f.controls)
    }
}
