//! A frame socket's statistics, and the `<path>.stats` endpoint that serves them to other
//! processes as JSON or Prometheus text.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::IpcError;
use super::{Shared, stats_of};
use crate::metrics::{HopMetrics, MetricsSnapshot, Window};

/// How long a statistics connection may take to send its request.
pub(super) const REQUEST_WAIT: Duration = Duration::from_secs(2);
/// Largest statistics answer a client reads.
const MAX_ANSWER: u64 = 16 << 20;

/// A [`FrameSocket`](super::FrameSocket)'s statistics ([`FrameSocket::metrics`](super::FrameSocket::metrics),
/// [`fetch_metrics`]).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FrameSocketMetrics {
    /// The frame socket's path.
    pub path: String,
    /// Frames published, copied into a memfd to be sent, sent to consumers.
    pub published: u64,
    pub copied: u64,
    pub served: u64,
    /// Leases open now, and frames held for consumers besides the latest.
    pub leases: u64,
    pub held_frames: u64,
    /// Leases ended early (held past `max_hold`, or more frames held than allowed).
    pub revoked: u64,
    /// Consumers that got no frame.
    pub unserved: u64,
    /// How long consumers held their leases: send to close (or revocation).
    pub hold: Window,
    /// Hop times of the frames sent, sensor to the send, and their copies.
    pub hops: HopMetrics,
    /// The serving process: its cameras and itself (copies, dma-buf syncs).
    pub snapshot: MetricsSnapshot,
}

impl FrameSocketMetrics {
    /// As JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// As Prometheus text: the frame socket's counters (label `socket`), hold times, hop times,
    /// and the snapshot of its process.
    pub fn prometheus_text(&self) -> String {
        crate::metrics::frame_socket_text(self)
    }
}

/// The statistics endpoint of the frame socket at `path`: `<path>.stats`.
pub fn stats_path(path: impl AsRef<Path>) -> PathBuf {
    let mut p = path.as_ref().as_os_str().to_owned();
    p.push(".stats");
    PathBuf::from(p)
}

pub(super) fn metrics_of(shared: &Shared) -> FrameSocketMetrics {
    use crate::metrics::RingWindow as _;
    let s = stats_of(shared);
    FrameSocketMetrics {
        path: shared.path.display().to_string(),
        published: s.published,
        copied: s.copied,
        served: s.served,
        leases: s.leases as u64,
        held_frames: s.held_frames as u64,
        revoked: s.revoked,
        unserved: s.unserved,
        hold: shared.hold.window(),
        hops: HopMetrics::of(&shared.hops),
        snapshot: crate::metrics::snapshot(),
    }
}

/// Reads the request (`json` or `prometheus`) and writes the answer, then closes.
pub(super) fn answer(shared: &Shared, socket: OwnedFd) {
    let mut stream = UnixStream::from(socket);
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let mut request = [0u8; 64];
    let n = stream.read(&mut request).unwrap_or(0);
    let request = std::str::from_utf8(&request[..n]).unwrap_or("").trim();
    let metrics = metrics_of(shared);
    let body = if request.starts_with('p') {
        metrics.prometheus_text()
    } else {
        metrics.to_json()
    };
    let _ = stream.write_all(body.as_bytes());
}

fn fetch(path: &Path, request: &str) -> Result<String, IpcError> {
    let mut stream = UnixStream::connect(stats_path(path))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(request.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut body = String::new();
    stream.take(MAX_ANSWER).read_to_string(&mut body)?;
    Ok(body)
}

/// The statistics of the frame socket at `path` (served by another process), from its
/// `<path>.stats` endpoint.
pub fn fetch_metrics(path: impl AsRef<Path>) -> Result<FrameSocketMetrics, IpcError> {
    let json = fetch(path.as_ref(), "json\n")?;
    serde_json::from_str(&json).map_err(|_| IpcError::Malformed("frame socket statistics JSON"))
}

/// [`fetch_metrics`] as Prometheus text.
pub fn fetch_metrics_text(path: impl AsRef<Path>) -> Result<String, IpcError> {
    fetch(path.as_ref(), "prometheus\n")
}
