//! The event thread of a started camera: serves the bridge's start/stop requests and drives
//! the control schedule from the receiver's `FRAME_SYNC` events.
//!
//! It runs whether or not anybody reads frames, so control writes stay on their frames even
//! when the consumer lags. It waits on the bridge (priority), the video node (priority) and a
//! wake pipe through the `styx-graph` reactor.
//!
//! Faults are recorded in [`Health`], which ends the stream: the bridge going away (`ENODEV`,
//! hang-up) ends the thread too; the video node going away or the sensor no longer taking
//! control writes leave it serving the bridge, whose stop request follows. A start served after the
//! bridge stopped waiting for it (the acknowledgement fails with `ESTALE`) puts the sensor
//! back in standby, so it never streams into a receiver that is not running.

use std::future::poll_fn;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::thread::JoinHandle;
use std::time::Duration;

use styx_graph::rt::{self, AsyncFd, Ready};
use styx_kernel::bus::StreamAction;
use styx_kernel::event::EventKind;

use crate::device::{BridgeDevice, CaptureDevice, EventSource};
use crate::embedded::EmbeddedCapture;
use crate::health::{Fault, Health};
use crate::stream::SensorSide;

/// How long the video node is left out of the wait after it reported an error (it does while
/// not streaming, e.g. while `STREAMON` waits for the start acknowledgement).
const VIDEO_ERROR_BACKOFF: Duration = Duration::from_millis(5);

pub(crate) struct EventThread {
    stop: Arc<AtomicBool>,
    wake: Option<PipeWriter>,
    handle: Option<JoinHandle<()>>,
}

enum Which {
    Bridge(Ready),
    Video(Ready),
    Wake,
    Timer,
}

/// What the thread serves.
pub(crate) struct EventSources {
    pub(crate) bridge: Arc<dyn BridgeDevice>,
    /// The capture node, when frame-start events are subscribed.
    pub(crate) video: Option<Arc<dyn CaptureDevice>>,
    pub(crate) sensor: Arc<dyn SensorSide>,
    pub(crate) embedded: Option<Arc<EmbeddedCapture>>,
    pub(crate) health: Arc<Health>,
}

impl EventThread {
    /// Starts serving.
    pub(crate) fn spawn(sources: EventSources) -> std::io::Result<Self> {
        let (reader, writer) = std::io::pipe()?;
        rt::set_nonblocking(std::os::fd::AsFd::as_fd(&reader))?;
        let wake = AsyncFd::new(reader)?;
        let bridge_fd = AsyncFd::new(Arc::clone(&sources.bridge))?;
        let video_fd = match &sources.video {
            Some(v) => Some(AsyncFd::new(EventSource(Arc::clone(v)))?),
            None => None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let keep = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("styx-native-events".into())
            .spawn(move || {
                rt::block_on(run(sources, bridge_fd, video_fd, wake, keep));
            })?;
        Ok(Self {
            stop,
            wake: Some(writer),
            handle: Some(handle),
        })
    }

    /// Stops the thread and waits for it.
    pub(crate) fn join(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mut w) = self.wake.take() {
            let _ = w.write_all(&[1]);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
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
        match s.bridge.acknowledge(&req, result) {
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

/// Drives the control schedule from every pending frame-start event. `Err` ends the thread.
fn serve_frame_starts(s: &EventSources) -> Result<(), Fault> {
    let Some(v) = &s.video else { return Ok(()) };
    loop {
        match v.dequeue_event() {
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

async fn run(
    s: EventSources,
    bridge_fd: AsyncFd<Arc<dyn BridgeDevice>>,
    video_fd: Option<AsyncFd<EventSource>>,
    wake: AsyncFd<PipeReader>,
    stop: Arc<AtomicBool>,
) {
    let bridge_interest = s.bridge.request_interest();
    let video_interest = s.video.as_ref().map(|v| v.event_interest());
    let mut video_backoff = false;
    let mut video_gone = false;
    while !stop.load(Ordering::Acquire) {
        // Serve whatever is pending first (events may predate the wait).
        if let Err(f) = serve_requests(&s) {
            s.health.fail(f);
            return;
        }
        if !video_gone && let Err(f) = serve_frame_starts(&s) {
            s.health.fail(f);
            video_gone = true;
        }
        // The previous frame's embedded line has arrived by the next frame start.
        if let Some(e) = &s.embedded {
            e.drain();
        }
        let which = {
            let mut b = bridge_fd.ready(bridge_interest);
            let mut w = wake.readable();
            let mut v = match (&video_fd, video_interest) {
                (Some(fd), Some(i)) if !video_backoff && !video_gone => Some(fd.ready(i)),
                _ => None,
            };
            let mut timer = video_backoff.then(|| rt::sleep(VIDEO_ERROR_BACKOFF));
            poll_fn(|cx| {
                if let Poll::Ready(r) = Pin::new(&mut b).poll(cx) {
                    return Poll::Ready(Which::Bridge(r.unwrap_or(Ready::ERROR)));
                }
                if let Some(v) = v.as_mut()
                    && let Poll::Ready(r) = Pin::new(v).poll(cx)
                {
                    return Poll::Ready(Which::Video(r.unwrap_or(Ready::ERROR)));
                }
                if Pin::new(&mut w).poll(cx).is_ready() {
                    return Poll::Ready(Which::Wake);
                }
                if let Some(t) = timer.as_mut()
                    && Pin::new(t).poll(cx).is_ready()
                {
                    return Poll::Ready(Which::Timer);
                }
                Poll::Pending
            })
            .await
        };
        video_backoff = false;
        match which {
            Which::Wake => {
                let mut buf = [0u8; 16];
                let _ = wake.get_ref().read(&mut buf);
            }
            Which::Bridge(r) if r.is_hangup() || r.is_error() => {
                // A request may still be pending next to the error; then the bridge is gone.
                let fault = serve_requests(&s)
                    .err()
                    .unwrap_or_else(|| Fault::Disconnected("the sensor bridge went away".into()));
                s.health.fail(fault);
                return;
            }
            Which::Video(r) if r.is_hangup() => {
                // Keep serving the bridge: the receiver's stop request follows.
                s.health
                    .fail(Fault::Disconnected("the capture node went away".into()));
                video_gone = true;
            }
            Which::Video(r) if r.is_error() && !r.is_priority() => video_backoff = true,
            Which::Bridge(_) | Which::Video(_) | Which::Timer => {}
        }
    }
}
