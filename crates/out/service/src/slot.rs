//! One camera's shared state: what the worker thread reports and what the control socket reads.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use styx_controls::{Control, ControlValue, FrameMetadata, Update};
use styx_frame::FrameLease;
use styx_native_run::Selection;
use styx_select::{Backend, RateReport, Selected};
use styx_socket::{Accepted, HandshakePlan};

use crate::config::CameraConfig;

/// Whether a camera's frames go to readers in other processes, and why not when they don't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publish {
    /// Not streaming yet: no frame has come to publish.
    Waiting,
    /// Frames are published on the camera's socket.
    Publishing,
    /// The camera's frames cannot go to readers without a copy (not fd-backed, or the socket could
    /// not be made). Frames still reach the outlet and the status; nothing is copied to publish.
    NotPublishable(String),
}

impl fmt::Display for Publish {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Publish::Waiting => f.write_str("waiting"),
            Publish::Publishing => f.write_str("publishing"),
            Publish::NotPublishable(why) => write!(f, "not-publishable ({why})"),
        }
    }
}

/// A camera's health as its backend reports it. Native cameras report it (the sensor's health
/// and reason, read when the camera is opened); the other backends do not yet, so theirs stays
/// `Unknown`. `Degraded` is a usable camera with a reason, never a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// No backend has reported health for this camera.
    Unknown,
    /// The backend reports the camera fine.
    Healthy,
    /// The backend reports the camera working, with a reason it is not fully healthy. Served.
    Degraded(String),
    /// The backend reports the device failed, with its reason. The service keeps trying.
    Failed(String),
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Health::Unknown => f.write_str("unknown"),
            Health::Healthy => f.write_str("healthy"),
            Health::Degraded(why) => write!(f, "degraded ({why})"),
            Health::Failed(why) => write!(f, "failed ({why})"),
        }
    }
}

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

/// The frames the measured rate is taken over: the last this many capture timestamps.
pub const RATE_WINDOW: usize = 32;

/// The capture timestamps of the last [`RATE_WINDOW`] frames, in a fixed array: the measured rate
/// is computed from them with no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateWindow {
    stamps: [u64; RATE_WINDOW],
    /// Frames pushed so far (the array is filled from index 0, then wraps).
    count: u64,
}

impl RateWindow {
    pub const fn new() -> Self {
        Self {
            stamps: [0; RATE_WINDOW],
            count: 0,
        }
    }

    /// Adds a frame's capture timestamp, `CLOCK_MONOTONIC` nanoseconds.
    pub fn push(&mut self, timestamp_ns: u64) {
        self.stamps[(self.count % RATE_WINDOW as u64) as usize] = timestamp_ns;
        self.count += 1;
    }

    /// The rate over the window, in thousandths of a frame per second: the frames after the
    /// oldest one, over the time from it to the newest. `None` with fewer than two frames, or with
    /// no time between them.
    pub fn measured_milli(&self) -> Option<u32> {
        let n = self.count.min(RATE_WINDOW as u64);
        if n < 2 {
            return None;
        }
        let newest = self.stamps[((self.count - 1) % RATE_WINDOW as u64) as usize];
        let oldest = self.stamps[((self.count - n) % RATE_WINDOW as u64) as usize];
        let span = newest.checked_sub(oldest).filter(|span| *span > 0)?;
        // (n - 1) frames over `span` ns, in milli-fps: (n - 1) * 1e12 / span.
        let milli = u128::from(n - 1) * 1_000_000_000_000 / u128::from(span);
        u32::try_from(milli).ok()
    }
}

impl Default for RateWindow {
    fn default() -> Self {
        Self::new()
    }
}

/// The frame ring's window for a camera: how many of the newest frames are kept for readers, the
/// pool's buffers, the hardware reserve, and the frames given back after leaving the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeepWindow {
    /// The keep window in force (0 until the camera publishes).
    pub keep: usize,
    /// Buffers in the camera's pool (0 until the camera publishes).
    pub buffers: usize,
    /// Buffers the camera keeps for itself to stream (0 until the camera publishes).
    pub hardware_reserve: usize,
    /// Frames given back to the pool after leaving the window.
    pub reclaimed: u64,
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
    /// The camera's socket for readers in other processes.
    pub socket: Option<PathBuf>,
    /// Whether and how the camera's frames are published.
    pub publish: Publish,
    /// Readers connected to the camera's socket now.
    pub readers: usize,
    /// Frames published to readers, since the service started.
    pub published: u64,
    /// Frames not published because the ring had no free buffer for them.
    pub publish_dropped: u64,
    /// The camera's health, as its backend reports it (`Unknown` until a backend does).
    pub health: Health,
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
    /// Times the service stopped a sensor and receiver a previous owner left started, since the
    /// service started (see `worker`).
    pub recoveries: u64,
    /// The last such stop: what was stopped, or what refused.
    pub last_recovery: Option<String>,
    /// Frames delivered since the service started.
    pub frames: u64,
    /// Frame identities skipped (a gap in the stream), since the service started.
    pub skipped: u64,
    /// The last frame delivered.
    pub last_frame: Option<FrameSummary>,
    /// The frame rate asked for, in milli-fps: the config's `fps_milli`, or the last frame-rate
    /// control accepted. `None` when nothing asks for one.
    pub requested_milli: Option<u32>,
    /// The rate the backend reports its frames run at, in milli-fps (`None` when it does not know).
    pub achieved_milli: Option<u32>,
    /// Whether the backend clamped the rate asked for to the sensor mode's limit.
    pub rate_clamped: bool,
    /// The achieved rate in the last frame's metadata, in milli-fps (`None` before a frame, or
    /// when the metadata does not carry one).
    pub achieved_meta_milli: Option<u32>,
    /// The rate measured from the capture timestamps of the last [`RATE_WINDOW`] frames.
    pub measured_milli: Option<u32>,
    /// The frame ring's keep window, pool size, hardware reserve and reclaimed count.
    pub keep: KeepWindow,
    /// The capture timestamps the measured rate is taken over.
    pub rate_window: RateWindow,
    /// The sensor mode a native camera runs in, and why it was chosen (`None` for the other
    /// backends, and before a native camera starts).
    pub mode: Option<Selection>,
    /// Frames the backend delivered with zero bytes (every plane of the frame's layout is empty),
    /// since the service started. The frame is still counted in `frames` and given on.
    pub empty_frames: u64,
}

impl CameraStatus {
    /// Why the achieved rate is not the requested one, in one line, when it is not. `None` when
    /// they agree, or when there is nothing to compare.
    pub fn rate_reason(&self) -> Option<String> {
        match (self.requested_milli, self.achieved_milli) {
            (None, _) => None,
            (Some(asked), Some(got)) if asked != got => {
                let why = if self.rate_clamped {
                    "clamped to the sensor mode's frame length limit"
                } else {
                    "the frame length is whole lines, so the rate rounds"
                };
                Some(format!("asked {asked} milli-fps, runs at {got}: {why}"))
            }
            (Some(_), None) => Some("the backend does not report its rate".to_string()),
            _ => None,
        }
    }
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
    /// A reader connected to the camera's socket asks for its handshake plan. The reply is the
    /// publisher's generation and the plan, or `None` when the camera is not publishing right now
    /// (the reader is closed, and connects again).
    Handshake(Sender<Option<(u64, HandshakePlan)>>),
    /// A reader answered its handshake: attach it to the publisher of the generation it was
    /// planned on. Any other generation is closed.
    Attach(u64, Accepted),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Slot {
    pub fn new(id: String, settings: CameraConfig, commands: Sender<Command>) -> Self {
        let status = CameraStatus {
            id: id.clone(),
            socket: None,
            publish: Publish::Waiting,
            readers: 0,
            published: 0,
            publish_dropped: 0,
            health: Health::Unknown,
            owner: None,
            reason: None,
            state: State::Absent,
            starts: 0,
            failures: 0,
            total_failures: 0,
            next_delay: None,
            last_error: None,
            recoveries: 0,
            last_recovery: None,
            frames: 0,
            skipped: 0,
            last_frame: None,
            requested_milli: settings.fps_milli,
            achieved_milli: None,
            rate_clamped: false,
            achieved_meta_milli: None,
            measured_milli: None,
            keep: KeepWindow::default(),
            rate_window: RateWindow::new(),
            mode: None,
            empty_frames: 0,
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

    /// Records a stop of the devices a previous owner left started, and what it reported.
    pub(crate) fn recovered(&self, note: String) {
        let mut status = lock(&self.status);
        status.recoveries += 1;
        status.last_recovery = Some(note);
    }

    pub(crate) fn reset_failures(&self) {
        lock(&self.status).failures = 0;
    }

    pub(crate) fn set_socket(&self, socket: Option<PathBuf>, publish: Publish) {
        let mut status = lock(&self.status);
        status.socket = socket;
        status.publish = publish;
    }

    pub(crate) fn set_publish(&self, publish: Publish) {
        let mut status = lock(&self.status);
        // With no socket there is nothing to publish on: the reason the socket could not be
        // made stays in the status until a socket exists.
        if status.socket.is_none() && matches!(status.publish, Publish::NotPublishable(_)) {
            return;
        }
        status.publish = publish;
    }

    pub(crate) fn set_health(&self, health: Health) {
        lock(&self.status).health = health;
    }

    pub(crate) fn set_readers(&self, readers: usize) {
        lock(&self.status).readers = readers;
    }

    pub(crate) fn set_requested(&self, milli: Option<u32>) {
        lock(&self.status).requested_milli = milli;
    }

    /// The backend's rate, as it reports it now.
    pub(crate) fn set_rate(&self, rate: RateReport) {
        let mut status = lock(&self.status);
        status.achieved_milli = rate.achieved_milli;
        status.rate_clamped = rate.clamped;
    }

    /// The mode choice a native camera started with.
    pub(crate) fn set_mode(&self, mode: Selection) {
        lock(&self.status).mode = Some(mode);
    }

    pub(crate) fn set_keep(&self, keep: KeepWindow) {
        lock(&self.status).keep = keep;
    }

    pub(crate) fn published(&self, dropped: bool) {
        let mut status = lock(&self.status);
        if dropped {
            status.publish_dropped += 1;
        } else {
            status.published += 1;
        }
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
        if lease.layouts().iter().all(|plane| plane.len == 0) {
            status.empty_frames += 1;
        }
        status.rate_window.push(lease.timestamp_ns());
        status.measured_milli = status.rate_window.measured_milli();
        status.achieved_meta_milli =
            (meta.achieved_fps_milli != 0).then_some(meta.achieved_fps_milli);
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

#[cfg(test)]
mod tests {
    use super::*;
    use styx_format::{FourCc, MediaFormat, Resolution};
    use styx_frame::{BufferSize, FrameEntry, FramePool, Layouts, PlaneLayout};

    fn status(requested: Option<u32>, achieved: Option<u32>, clamped: bool) -> CameraStatus {
        let mut s = Slot::new(
            "cam".to_string(),
            CameraConfig::default(),
            std::sync::mpsc::channel().0,
        )
        .snapshot();
        s.requested_milli = requested;
        s.achieved_milli = achieved;
        s.rate_clamped = clamped;
        s
    }

    #[test]
    fn the_measured_rate_is_over_the_window_of_capture_timestamps() {
        let mut window = RateWindow::new();
        assert_eq!(window.measured_milli(), None);
        // 120 fps: a frame each 8 333 333 ns, more frames than the window holds.
        for i in 0..(RATE_WINDOW as u64 * 3) {
            window.push(1_000_000_000 + i * 8_333_333);
        }
        let measured = window.measured_milli().expect("a rate");
        // (n - 1) frames over the span: 31 intervals of 8 333 333 ns is 119.9999 fps.
        assert!(
            (measured as i64 - 120_000).abs() < 10,
            "measured {measured}"
        );
    }

    #[test]
    fn a_window_with_no_time_between_frames_has_no_rate() {
        let mut window = RateWindow::new();
        window.push(5);
        window.push(5);
        assert_eq!(window.measured_milli(), None);
    }

    #[test]
    fn the_reason_names_a_clamp_and_is_absent_when_the_rates_agree() {
        assert_eq!(status(None, Some(119_880), true).rate_reason(), None);
        assert_eq!(
            status(Some(120_000), Some(120_000), false).rate_reason(),
            None
        );
        let clamped = status(Some(120_000), Some(114_800), true)
            .rate_reason()
            .expect("a reason");
        assert!(
            clamped.contains("clamped to the sensor mode's frame length limit"),
            "{clamped}"
        );
        assert!(
            clamped.contains("asked 120000") && clamped.contains("114800"),
            "{clamped}"
        );
        let rounded = status(Some(120_000), Some(119_984), false)
            .rate_reason()
            .expect("a reason");
        assert!(rounded.contains("whole lines"), "{rounded}");
        assert_eq!(
            status(Some(120_000), None, false).rate_reason().as_deref(),
            Some("the backend does not report its rate")
        );
    }

    /// A frame of `id` whose layout has the given plane lengths, from a pool made for the test.
    fn lease(
        pool: &FramePool<FrameMetadata>,
        id: u64,
        planes: &[usize],
    ) -> FrameLease<FrameMetadata> {
        let layouts: Vec<PlaneLayout> = planes
            .iter()
            .map(|&len| PlaneLayout::new(0, 8, len))
            .collect();
        let format =
            MediaFormat::with_defaults(FourCc::GREY, Resolution::new(8, 8).expect("a resolution"));
        let entry = FrameEntry::new(
            id,
            1_000 + id,
            format,
            Layouts::new(&layouts).expect("planes"),
        );
        pool.acquire(entry).expect("a free buffer")
    }

    #[test]
    fn a_frame_with_no_bytes_is_counted_as_empty_and_others_are_not() {
        let pool = FramePool::<FrameMetadata>::host(4, BufferSize::new(64, 0)).expect("a pool");
        let slot = Slot::new(
            "cam".to_string(),
            CameraConfig::default(),
            std::sync::mpsc::channel().0,
        );
        slot.frame(&lease(&pool, 1, &[64]));
        slot.frame(&lease(&pool, 2, &[0]));
        slot.frame(&lease(&pool, 3, &[]));
        slot.frame(&lease(&pool, 4, &[32, 0]));
        let status = slot.snapshot();
        assert_eq!(status.frames, 4);
        assert_eq!(status.empty_frames, 2);
        assert_eq!(status.last_frame.map(|f| f.id), Some(4));
    }
}
