//! Replays a recording as a camera (see [`crate::replay`]), with playback controls
//! ([`super::replay_controls`]): play and pause, speed, loop, seek, step.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_capture::prelude::*;

use super::control_plane::ControlPlane;
use super::handle::{CaptureHandle, CaptureQueue, WorkerHandle, enqueue_capture_frame};
use super::replay_controls::{ReplayControls, Seek};
use super::request::CaptureError;
use super::tunables::StyxConfig;
use crate::metrics::StageMetrics;
use crate::replay::open_source;
use crate::replay::{RecordingFrames, ReplayError, ReplayPacing, ReplayTimestamps, replay_options};
use crate::{BackendHandle, BackendKind, ProbedBackend};

/// The longest the worker waits without looking at its stop channel.
const POLL: Duration = Duration::from_millis(50);

pub(super) fn start_replay(
    backend: &ProbedBackend,
    mode: Mode,
    interval: Option<Interval>,
    descriptor: CaptureDescriptor,
    config: &StyxConfig,
    queue: Option<CaptureQueue>,
) -> Result<CaptureHandle, CaptureError> {
    let BackendHandle::Replay {
        path,
        pacing,
        loop_forever,
    } = &backend.handle
    else {
        return Err(CaptureError::BackendUnavailable(BackendKind::Replay));
    };
    let (path, pacing, loop_forever) = (path.clone(), *pacing, *loop_forever);
    let (timestamps, hold_at_end, isp, fps) = replay_options(&backend.properties);
    #[cfg(not(feature = "replay-softisp"))]
    if isp.is_some() {
        return Err(CaptureError::Backend(
            "replay: processing raw frames needs feature `replay-softisp`".into(),
        ));
    }
    let replay_error = |err: ReplayError| CaptureError::Backend(format!("replay: {err}"));
    // Fail at start, not in the worker, for a missing or foreign file.
    open_source(&path, fps).map_err(replay_error)?;

    let capture = config.capture_tunables();
    // Unpaced replay delivers every frame, so it waits for room instead of dropping; so does
    // a held replay (a player), whose speed 0 means as fast as the consumers.
    let overflow = match (pacing, hold_at_end) {
        (ReplayPacing::Unpaced, _) | (_, true) => QueueOverflow::Backpressure,
        (ReplayPacing::Realtime, false) => capture.queue_overflow,
    };
    let (tx, rx) = queue
        .unwrap_or_else(|| styx_core::queue::bounded_with(capture.queue_depth.max(1), overflow));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let worker_error = Arc::new(Mutex::new(None));
    let worker_error_for_thread = worker_error.clone();
    let send_timeout = Duration::from_millis(capture.queue_send_timeout_ms);
    let fallback_interval = interval
        .map(|i| Duration::from_secs_f32(1.0 / i.fps().max(0.01)))
        .unwrap_or(Duration::from_millis(33));
    let speed = match pacing {
        ReplayPacing::Realtime => 1.0,
        ReplayPacing::Unpaced => 0.0,
    };
    let controls = Arc::new(ReplayControls::new(speed, loop_forever));
    // The length, for the duration and frame count controls: one scan, beside the playback.
    {
        let (path, controls) = (path.clone(), controls.clone());
        thread::spawn(move || {
            if let Ok((frames, duration)) = scan(&path, fps) {
                controls.set_length(duration, frames);
            }
        });
    }
    let player_controls = controls.clone();
    let worker = thread::spawn(move || {
        let mut player = Player {
            path,
            fps,
            controls: player_controls,
            timestamps,
            hold_at_end,
            fallback_interval,
            send_timeout,
            frames: None,
            first: None,
            index: 0,
            last_raw: 0,
            last_delta: fallback_interval,
            loop_offset: 0,
            anchor: None,
            pending: None,
            #[cfg(feature = "replay-softisp")]
            isp: isp.map(super::replay_isp::ReplayProcessor::new),
        };
        if let Err(err) = player.run(&tx, &stop_rx) {
            crate::trace::warn!(backend = "replay", error = %err, "replay failed");
            *worker_error_for_thread.lock() = Some(CaptureError::Backend(format!("replay: {err}")));
        }
        // End of the recording: queued frames drain, then receivers see `Closed`.
        tx.close();
    });
    Ok(CaptureHandle {
        backend: BackendKind::Replay,
        control: ControlPlane::Replay(controls),
        descriptor,
        mode,
        interval,
        rx,
        stop_tx: Some(stop_tx),
        worker: Some(WorkerHandle::Thread(worker)),
        aux_workers: Vec::new(),
        #[cfg(feature = "libcamera")]
        libcamera_idle_stop_allowed: false,
        #[cfg(feature = "libcamera")]
        libcamera_stop_when_idle: false,
        metrics: StageMetrics::default(),
        external_backings: Vec::new(),
        worker_error,
        control_error: Arc::new(Mutex::new(None)),
        shutdown_stats: Default::default(),
        retry_metrics: Default::default(),
        sequence_gaps: Default::default(),
        live: Default::default(),
    })
}

/// The recording's frames and its length (first to last timestamp).
fn scan(path: &std::path::Path, fps: Option<f32>) -> Result<(u64, Duration), ReplayError> {
    let (_, frames) = open_source(path, fps)?;
    let (mut n, mut first, mut last) = (0u64, None, 0u64);
    for f in frames {
        let ts = f?.meta().timestamp;
        first.get_or_insert(ts);
        last = ts;
        n += 1;
    }
    Ok((
        n,
        Duration::from_nanos(last.saturating_sub(first.unwrap_or(last))),
    ))
}

/// What the worker does next, from the controls.
struct Turn {
    playing: bool,
    speed: f32,
    looping: bool,
    seek: Option<Seek>,
    step: bool,
    stopped: bool,
    generation: u64,
}

struct Player {
    path: std::path::PathBuf,
    /// The rate of video files without one.
    fps: Option<f32>,
    controls: Arc<ReplayControls>,
    timestamps: ReplayTimestamps,
    hold_at_end: bool,
    fallback_interval: Duration,
    send_timeout: Duration,
    /// The pass being played (`None`: open the recording again).
    frames: Option<RecordingFrames>,
    /// The recording's first timestamp (positions are from it).
    first: Option<u64>,
    /// The next frame's index in the pass.
    index: u64,
    /// The last frame's recorded timestamp, and the interval before it.
    last_raw: u64,
    last_delta: Duration,
    /// Added to recorded timestamps so they keep increasing across loops.
    loop_offset: u64,
    /// When the frame with this recorded timestamp was (or would have been) due: frames are
    /// paced from it at the speed. Cleared by anything that breaks the pace.
    anchor: Option<(Instant, u64)>,
    /// A frame read but not delivered yet (its time had not come), with its index.
    pending: Option<(FrameLease, u64)>,
    /// Raw frames processed by the software ISP (`ReplaySourceConfig::soft_isp`).
    #[cfg(feature = "replay-softisp")]
    isp: Option<super::replay_isp::ReplayProcessor>,
}

impl Player {
    fn turn(&self) -> Turn {
        let mut r = self.controls.requests.lock();
        let step = r.steps > 0;
        if step {
            r.steps -= 1;
        }
        Turn {
            playing: r.playing,
            speed: r.speed,
            looping: r.looping,
            seek: r.seek.take(),
            step,
            stopped: r.stopped,
            generation: r.generation,
        }
    }

    /// Waits up to `wait` for a control to change (or the poll interval).
    fn wait_change(&self, generation: u64, wait: Duration) {
        let mut r = self.controls.requests.lock();
        if r.generation == generation {
            let _ = self.controls.changed.wait_for(&mut r, wait.min(POLL));
        }
    }

    fn run(
        &mut self,
        tx: &BoundedTx<FrameLease>,
        stop_rx: &mpsc::Receiver<()>,
    ) -> Result<(), ReplayError> {
        loop {
            if stop_rx.try_recv().is_ok() {
                return Ok(());
            }
            let t = self.turn();
            if t.stopped {
                return Ok(());
            }
            // A seek shows its frame even while paused.
            let mut show = t.step;
            if let Some(seek) = t.seek {
                self.seek(seek)?;
                show = true;
            }
            if !t.playing && !show {
                self.controls.set_state(1);
                self.anchor = None;
                self.wait_change(t.generation, POLL);
                continue;
            }
            let (frame, index) = match self.pending.take() {
                Some(p) => p,
                None => match self.next()? {
                    Some(p) => p,
                    None if t.looping => {
                        self.next_loop();
                        continue;
                    }
                    None if self.hold_at_end => {
                        self.controls.set_state(2);
                        self.anchor = None;
                        self.wait_change(t.generation, POLL);
                        continue;
                    }
                    None => return Ok(()),
                },
            };
            let raw = frame.meta().timestamp.saturating_sub(self.loop_offset);
            // Paced: wait for its time (a control change in the meantime is looked at first).
            if t.speed > 0.0 && t.playing && !show {
                let (at, from) = *self.anchor.get_or_insert((Instant::now(), raw));
                let due = at + Duration::from_nanos(raw.saturating_sub(from)).div_f32(t.speed);
                let wait = due.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    self.pending = Some((frame, index));
                    self.wait_change(t.generation, wait);
                    continue;
                }
            } else {
                self.anchor = None;
            }
            self.controls.set_state(if t.playing { 0 } else { 1 });
            let first = self.first.unwrap_or(raw);
            #[cfg(feature = "replay-softisp")]
            let frame = match self.isp.as_mut() {
                Some(isp) => isp.process(frame)?,
                None => frame,
            };
            if self.deliver(frame, t.speed, tx, stop_rx) {
                return Ok(());
            }
            self.controls
                .delivered(index, Duration::from_nanos(raw.saturating_sub(first)));
        }
    }

    /// The next frame of the pass and its index (`None` at the end).
    fn next(&mut self) -> Result<Option<(FrameLease, u64)>, ReplayError> {
        if self.frames.is_none() {
            let (_, frames) = open_source(&self.path, self.fps)?;
            self.frames = Some(frames.with_timestamp_offset(self.loop_offset));
            self.index = 0;
        }
        let Some(frame) = self.frames.as_mut().and_then(Iterator::next) else {
            return Ok(None);
        };
        let frame = frame?;
        let raw = frame.meta().timestamp.saturating_sub(self.loop_offset);
        self.first.get_or_insert(raw);
        if raw > self.last_raw && self.last_raw != 0 && self.index > 0 {
            self.last_delta = Duration::from_nanos(raw - self.last_raw);
        }
        self.last_raw = raw;
        let index = self.index;
        self.index += 1;
        Ok(Some((frame, index)))
    }

    /// Goes back to the start of the recording and forward to `seek`: the frame there is read
    /// and kept to deliver next.
    fn seek(&mut self, seek: Seek) -> Result<(), ReplayError> {
        self.frames = None;
        self.pending = None;
        self.anchor = None;
        self.loop_offset = 0;
        self.last_raw = 0;
        let mut last = None;
        while let Some((frame, index)) = self.next()? {
            let raw = frame.meta().timestamp;
            let first = self.first.unwrap_or(raw);
            let reached = match seek {
                Seek::Time(ns) => raw.saturating_sub(first) >= ns,
                Seek::Frame(n) => index >= n,
            };
            if reached {
                self.pending = Some((frame, index));
                return Ok(());
            }
            last = Some((frame, index));
        }
        // Past the end: the last frame.
        self.pending = last;
        Ok(())
    }

    fn next_loop(&mut self) {
        let gap = if self.last_delta.is_zero() {
            self.fallback_interval
        } else {
            self.last_delta
        };
        // The next pass's first frame follows the last one by one frame interval.
        let first = self.first.unwrap_or(0);
        self.loop_offset = (self.last_raw + self.loop_offset)
            .saturating_add(gap.as_nanos() as u64)
            .saturating_sub(first);
        self.frames = None;
        self.anchor = None;
    }

    /// Sends `frame` (as fast as the consumers take frames at speed 0); `true` when the
    /// replay should stop.
    fn deliver(
        &self,
        mut frame: FrameLease,
        speed: f32,
        tx: &BoundedTx<FrameLease>,
        stop_rx: &mpsc::Receiver<()>,
    ) -> bool {
        let now = Instant::now();
        let meta = frame.meta_mut();
        // Recorded metadata (and the native 3A record) is kept; only the delivery time is new,
        // and with rebased timestamps the frame's time is its delivery.
        meta.capture_instant = Some(now.into());
        if self.timestamps == ReplayTimestamps::Rebased {
            meta.timestamp = CaptureInstant::from(now).as_nanos();
            meta.clock = Some(TimestampClock::Monotonic);
        }
        if speed == 0.0 {
            send_every_frame(tx, frame, stop_rx)
        } else {
            enqueue_capture_frame(tx, frame, "replay", self.send_timeout)
        }
    }
}

/// Wait for room rather than dropping; returns `true` when the replay should stop.
fn send_every_frame(
    tx: &BoundedTx<FrameLease>,
    mut frame: FrameLease,
    stop_rx: &mpsc::Receiver<()>,
) -> bool {
    loop {
        match tx.send_timeout(frame, POLL) {
            SendWaitOutcome::Ok => return false,
            SendWaitOutcome::Closed(_) => return true,
            SendWaitOutcome::Timeout(back) => {
                if stop_rx.try_recv().is_ok() {
                    return true;
                }
                frame = back;
            }
        }
    }
}
