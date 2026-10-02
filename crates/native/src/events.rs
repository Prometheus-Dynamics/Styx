//! The event thread of a started camera: serves the bridge's start/stop requests, drives the
//! control schedule from the receiver's `FRAME_SYNC` events, and tells the frame stream when
//! a frame is ready.
//!
//! It is the only code that waits on the capture node, with its own `poll(2)` rather than the
//! shared reactor, because of how the receiver locks: `rp1-cfe` takes the node's lock in
//! `VIDIOC_STREAMON`/`STREAMOFF` and holds it while the bridge waits for the acknowledgement,
//! and `poll` on the node (`vb2_fop_poll`) takes the same lock. Anything that polls the node
//! then (the reactor thread re-polling a woken descriptor, a `DQEVENT`) blocks until the
//! bridge times out, and with it whatever should have served the bridge: every stop timed
//! out after 1 s that way, and a start that times out makes `rp1-cfe` oops in its error path.
//! So around `STREAMON`/`STREAMOFF` the camera [quiesces](EventThread::quiesce) the thread: it
//! then waits on the bridge and its wake pipe only, and touches the node again once
//! [resumed](EventThread::resume).
//!
//! Faults are recorded in [`Health`], which ends the stream: the bridge going away (`ENODEV`,
//! hang-up) ends the thread too; the capture node going away or the sensor no longer taking
//! control writes leave it serving the bridge, whose stop request follows. A start served after
//! the bridge stopped waiting for it (the acknowledgement fails with `ESTALE`) puts the sensor
//! back in standby, so it never streams into a receiver that is not running.

use std::io::{PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Waker;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use styx_kernel::bus::StreamAction;
use styx_kernel::event::EventKind;
use styx_kernel::{Wait, poll};

use crate::control::lock;
use crate::device::{BridgeDevice, CaptureDevice};
use crate::embedded::EmbeddedCapture;
use crate::health::{Fault, Health};
use crate::stream::SensorSide;

/// `STYX_NATIVE_DEBUG=1`: trace the event thread on stderr.
fn debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STYX_NATIVE_DEBUG").is_some_and(|v| v != "0"))
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if debug() {
            eprintln!("[styx-native events {:?}] {}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default(), format!($($arg)*));
        }
    };
}

/// How long the capture node is left out of the wait after it reported an error without
/// anything to read (it does while not streaming).
const VIDEO_ERROR_BACKOFF: Duration = Duration::from_millis(5);
/// How long [`EventThread::quiesce`] waits for the thread to let go of the capture node.
const QUIESCE_WAIT: Duration = Duration::from_millis(500);

/// Wakes the event thread (a byte on its pipe).
#[derive(Clone)]
pub(crate) struct Poke(Arc<Mutex<PipeWriter>>);

impl Poke {
    pub(crate) fn poke(&self) {
        let _ = lock(&self.0).write_all(&[1]);
    }
}

/// How the frame stream waits for a frame: it registers its waker and pokes the thread, which
/// then also waits for the node to be readable and wakes it.
pub(crate) struct FrameNotify {
    waker: Mutex<Option<Waker>>,
    want: AtomicBool,
    poke: Poke,
}

impl FrameNotify {
    /// Wake `waker` when a frame may be ready (or something ended the stream).
    pub(crate) fn register(&self, waker: &Waker) {
        {
            let mut w = lock(&self.waker);
            if !w.as_ref().is_some_and(|w| w.will_wake(waker)) {
                *w = Some(waker.clone());
            }
        }
        if !self.want.swap(true, Ordering::AcqRel) {
            self.poke.poke();
        }
    }

    /// Wakes the waiting stream, if any.
    pub(crate) fn wake(&self) {
        self.want.store(false, Ordering::Release);
        if let Some(w) = lock(&self.waker).take() {
            w.wake();
        }
    }

    fn wanted(&self) -> bool {
        self.want.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct GateState {
    quiet: bool,
    requested: u64,
    quiesced: u64,
}

/// Whether the thread may touch the capture node, with the handshake that confirms it let go.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

pub(crate) struct EventThread {
    stop: Arc<AtomicBool>,
    poke: Poke,
    gate: Arc<Gate>,
    notify: Arc<FrameNotify>,
    handle: Option<JoinHandle<()>>,
}

/// What the thread serves.
pub(crate) struct EventSources {
    pub(crate) bridge: Arc<dyn BridgeDevice>,
    pub(crate) video: Arc<dyn CaptureDevice>,
    /// Frame-start events are subscribed on the capture node.
    pub(crate) frame_sync: bool,
    pub(crate) sensor: Arc<dyn SensorSide>,
    pub(crate) embedded: Option<Arc<EmbeddedCapture>>,
    pub(crate) health: Arc<Health>,
}

impl EventThread {
    /// Starts serving, quiesced (the camera calls `STREAMON` next, then [`Self::resume`]).
    pub(crate) fn spawn(sources: EventSources) -> std::io::Result<Self> {
        let (reader, writer) = std::io::pipe()?;
        styx_graph::rt::set_nonblocking(reader.as_fd())?;
        let poke = Poke(Arc::new(Mutex::new(writer)));
        let notify = Arc::new(FrameNotify {
            waker: Mutex::new(None),
            want: AtomicBool::new(false),
            poke: poke.clone(),
        });
        let gate = Arc::new(Gate::default());
        lock(&gate.state).quiet = true;
        let stop = Arc::new(AtomicBool::new(false));
        let (keep, g, n) = (Arc::clone(&stop), Arc::clone(&gate), Arc::clone(&notify));
        let handle = std::thread::Builder::new()
            .name("styx-native-events".into())
            .spawn(move || run(&sources, &reader, &keep, &g, &n))?;
        Ok(Self {
            stop,
            poke,
            gate,
            notify,
            handle: Some(handle),
        })
    }

    /// How the frame stream waits for frames.
    pub(crate) fn notify(&self) -> Arc<FrameNotify> {
        Arc::clone(&self.notify)
    }

    /// Stops touching the capture node; returns once the thread confirmed (or after a bounded
    /// wait). Call before `STREAMON`/`STREAMOFF`.
    pub(crate) fn quiesce(&self) {
        let want = {
            let mut st = lock(&self.gate.state);
            st.quiet = true;
            st.requested += 1;
            st.requested
        };
        self.poke.poke();
        let deadline = Instant::now() + QUIESCE_WAIT;
        let mut st = lock(&self.gate.state);
        while st.quiesced < want && self.handle.as_ref().is_some_and(|h| !h.is_finished()) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self
                .gate
                .changed
                .wait_timeout(st, left.min(Duration::from_millis(10)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Touches the capture node again (after `STREAMON` returned).
    pub(crate) fn resume(&self) {
        lock(&self.gate.state).quiet = false;
        self.poke.poke();
    }

    /// Stops the thread and waits for it.
    pub(crate) fn join(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.poke.poke();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.notify.wake();
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Serves every pending bridge request. `Err` ends the thread.
fn serve_requests(s: &EventSources) -> Result<(), Fault> {
    loop {
        let req = match s.bridge.try_next_request() {
            Ok(Some(req)) => req,
            Ok(None) => return Ok(()),
            Err(e) => return Err(Fault::from_io("bridge stream request", &e)),
        };
        let result = s.sensor.serve(&req).map_err(|(errno, why)| {
            s.health.serve_failed(why);
            errno
        });
        let acked = s.bridge.acknowledge(&req, result);
        trace!(
            "request {:?} seq {} served {result:?} ack {acked:?}",
            req.action, req.sequence
        );
        match acked {
            Ok(()) => {
                s.health.acks.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) if crate::error::io_errno(&e) == Some(libc::ESTALE) => {
                // The bridge stopped waiting (timed out, or the start was abandoned): the
                // receiver is not running, so the sensor must not stream either.
                if req.action == StreamAction::Start && result.is_ok() {
                    s.sensor.standby();
                }
                s.health.late_acks.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => return Err(Fault::from_io("acknowledging a bridge request", &e)),
        }
    }
}

/// Drives the control schedule from every pending frame-start event. `Err`: the node is gone.
fn serve_frame_starts(s: &EventSources) -> Result<(), Fault> {
    loop {
        match s.video.dequeue_event() {
            Ok(Some(ev)) => {
                if let EventKind::FrameSync { frame_sequence } = ev.kind {
                    s.health.frame_syncs.fetch_add(1, Ordering::Relaxed);
                    s.health
                        .control_write(s.sensor.frame_start(u64::from(frame_sequence)));
                }
            }
            Ok(None) => return Ok(()),
            // Other errors (a queue that is not streaming) are left to the frame stream.
            Err(e) => {
                return match Fault::from_io("frame-start event", &e) {
                    f @ Fault::Disconnected(_) => Err(f),
                    _ => Ok(()),
                };
            }
        }
    }
}

/// Lets go of the capture node when asked to; returns whether it must be left alone.
fn check_gate(gate: &Gate) -> bool {
    let mut st = lock(&gate.state);
    if st.quiet && st.quiesced != st.requested {
        trace!("quiesced ({})", st.requested);
        st.quiesced = st.requested;
        gate.changed.notify_all();
    }
    st.quiet
}

fn run(s: &EventSources, wake: &PipeReader, stop: &AtomicBool, gate: &Gate, notify: &FrameNotify) {
    let video = s.video.as_ref();
    // On V4L2 nodes events come on the capture descriptor itself.
    let shared_fd = video.event_fd().as_raw_fd() == video.as_fd().as_raw_fd();
    let mut backoff_until: Option<Instant> = None;
    let mut video_gone = false;
    let fail = |f: Fault| {
        trace!("fault: {f:?}");
        s.health.fail(f);
        notify.wake();
    };
    while !stop.load(Ordering::Acquire) {
        let quiet = check_gate(gate);
        if let Err(f) = serve_requests(s) {
            fail(f);
            return;
        }
        let touch_video = !quiet && !video_gone;
        if touch_video
            && s.frame_sync
            && let Err(f) = serve_frame_starts(s)
        {
            fail(f);
            video_gone = true;
        }
        // The previous frame's embedded line has arrived by the next frame start.
        if touch_video && let Some(e) = &s.embedded {
            e.drain();
        }
        let now = Instant::now();
        let backoff = backoff_until.filter(|t| *t > now);
        let poll_video = touch_video && !video_gone && backoff.is_none();
        let frames = if notify.wanted() {
            Wait::READABLE
        } else {
            Wait::NONE
        };
        let events = if s.frame_sync {
            video.event_wait()
        } else {
            Wait::NONE
        };
        let mut fds = vec![
            (wake.as_fd(), Wait::READABLE),
            (s.bridge.as_fd(), s.bridge.request_wait()),
        ];
        if poll_video {
            if shared_fd {
                fds.push((video.as_fd(), frames.union(events)));
            } else {
                fds.push((video.as_fd(), frames));
                fds.push((video.event_fd(), events));
            }
        }
        let ready = poll(&fds, backoff.map(|t| t - now));
        trace!("quiet {quiet} polled {} fds: {ready:?}", fds.len());
        let ready = match ready {
            Ok(r) => r,
            Err(e) => {
                fail(Fault::from_io("waiting for events", &e.into()));
                return;
            }
        };
        if ready[0].readable {
            let mut buf = [0u8; 64];
            let _ = (&*wake).read(&mut buf);
        }
        let b = ready[1];
        if b.hangup || b.error {
            // A request may still be pending next to the error; then the bridge is gone.
            let fault = serve_requests(s)
                .err()
                .unwrap_or_else(|| Fault::Disconnected("the sensor bridge went away".into()));
            fail(fault);
            return;
        }
        let Some(v) = ready.get(2).copied() else {
            continue;
        };
        let e = ready.get(3).copied().unwrap_or_default();
        if v.hangup || e.hangup {
            // Keep serving the bridge: the receiver's stop request follows.
            fail(Fault::Disconnected("the capture node went away".into()));
            video_gone = true;
            continue;
        }
        if v.readable {
            notify.wake();
        }
        if v.error && !v.readable && !v.priority {
            // Not streaming, or the queue failed: the stream's next dequeue tells which.
            notify.wake();
            backoff_until = Some(Instant::now() + VIDEO_ERROR_BACKOFF);
        }
    }
}
