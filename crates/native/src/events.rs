//! The event thread of a started camera: serves the bridge's start/stop requests and drives
//! the control schedule from the receiver's `FRAME_SYNC` events.
//!
//! It runs whether or not anybody reads frames, so control writes stay on their frames even
//! when the consumer lags. It waits on the bridge (priority), the video node (priority) and a
//! wake pipe through the `styx-graph` reactor.

use std::future::poll_fn;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::Poll;
use std::thread::JoinHandle;
use std::time::Instant;

use styx_graph::rt::{self, AsyncFd};
use styx_kernel::bus::SensorBridge;
use styx_kernel::event::{EventKind, Events};
use styx_kernel::v4l2::VideoDevice;

use crate::embedded::EmbeddedCapture;
use crate::stream::SensorSide;

/// Counters the event thread keeps.
#[derive(Debug, Default)]
pub(crate) struct EventStats {
    pub(crate) acks: AtomicU64,
    pub(crate) frame_syncs: AtomicU64,
    /// Nanoseconds from the thread's start to the last start acknowledgement.
    pub(crate) last_start_ack_ns: AtomicU64,
}

pub(crate) struct EventThread {
    stop: Arc<AtomicBool>,
    wake: Option<PipeWriter>,
    handle: Option<JoinHandle<()>>,
    pub(crate) stats: Arc<EventStats>,
}

enum Which {
    Bridge,
    Video,
    Wake,
}

impl EventThread {
    /// Starts serving. `video` is the capture node's reactor registration (shared with the
    /// frame stream), when frame-start events are subscribed.
    pub(crate) fn spawn(
        bridge: Arc<SensorBridge>,
        video: Option<Arc<AsyncFd<Arc<VideoDevice>>>>,
        sensor: Arc<dyn SensorSide>,
        embedded: Option<Arc<EmbeddedCapture>>,
    ) -> std::io::Result<Self> {
        let (reader, writer) = std::io::pipe()?;
        rt::set_nonblocking(std::os::fd::AsFd::as_fd(&reader))?;
        let wake = AsyncFd::new(reader)?;
        let bridge_fd = AsyncFd::new(Arc::clone(&bridge))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(EventStats::default());
        let (keep, st) = (Arc::clone(&stop), Arc::clone(&stats));
        let handle = std::thread::Builder::new()
            .name("styx-native-events".into())
            .spawn(move || {
                rt::block_on(run(
                    bridge, bridge_fd, video, wake, sensor, embedded, keep, st,
                ));
            })?;
        Ok(Self {
            stop,
            wake: Some(writer),
            handle: Some(handle),
            stats,
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

#[allow(clippy::too_many_arguments)]
async fn run(
    bridge: Arc<SensorBridge>,
    bridge_fd: AsyncFd<Arc<SensorBridge>>,
    video: Option<Arc<AsyncFd<Arc<VideoDevice>>>>,
    wake: AsyncFd<PipeReader>,
    sensor: Arc<dyn SensorSide>,
    embedded: Option<Arc<EmbeddedCapture>>,
    stop: Arc<AtomicBool>,
    stats: Arc<EventStats>,
) {
    let t0 = Instant::now();
    while !stop.load(Ordering::Acquire) {
        // Serve whatever is pending first (events may predate the wait).
        while let Ok(Some(req)) = bridge.try_next_request() {
            let result = sensor.serve(&req);
            // ESTALE: the bridge gave up waiting (timeout) or never asked.
            let _ = bridge.acknowledge(&req, result);
            stats.acks.fetch_add(1, Ordering::Relaxed);
            if result.is_ok() {
                stats
                    .last_start_ack_ns
                    .store(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
        }
        if let Some(v) = &video {
            while let Ok(Some(ev)) = v.get_ref().dequeue_event() {
                if let EventKind::FrameSync { frame_sequence } = ev.kind {
                    stats.frame_syncs.fetch_add(1, Ordering::Relaxed);
                    sensor.frame_start(u64::from(frame_sequence));
                }
            }
        }
        // The previous frame's embedded line has arrived by the next frame start.
        if let Some(e) = &embedded {
            e.drain();
        }
        let which = {
            let mut b = bridge_fd.priority();
            let mut w = wake.readable();
            let mut v = video.as_ref().map(|v| v.priority());
            poll_fn(|cx| {
                if Pin::new(&mut b).poll(cx).is_ready() {
                    return Poll::Ready(Which::Bridge);
                }
                if let Some(v) = v.as_mut()
                    && Pin::new(v).poll(cx).is_ready()
                {
                    return Poll::Ready(Which::Video);
                }
                if Pin::new(&mut w).poll(cx).is_ready() {
                    return Poll::Ready(Which::Wake);
                }
                Poll::Pending
            })
            .await
        };
        if let Which::Wake = which {
            let mut buf = [0u8; 16];
            let _ = wake.get_ref().read(&mut buf);
        }
    }
}
