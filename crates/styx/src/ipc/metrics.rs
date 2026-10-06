//! A camera service's metrics for other processes: a client asks (a metrics request instead of
//! a frame request) and the service answers with its counters, its clients and the metrics of
//! every capture in its process, as JSON or Prometheus text in a memfd (no size limit).

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::connection::Connection;
use super::wire::{self, MetricsFormat, ServerMessage};
use super::{FrameClient, IpcError, socket};
use crate::metrics::{ConsumerMetrics, ConsumerStats, ServiceMetrics};

/// Longest metrics answer accepted (a service with many cameras and clients stays far below).
const MAX_METRICS: usize = 16 << 20;
const METRICS_TIMEOUT: Duration = Duration::from_secs(5);

/// The clients a camera service serves, for its metrics.
#[derive(Default)]
pub(super) struct Clients(Mutex<Vec<Weak<ConsumerStats>>>);

impl Clients {
    /// A client of `camera` connected over `conn`: counted until its stats are dropped.
    pub(super) fn add(&self, id: u64, camera: &str, conn: &mut Connection) {
        let pid = conn
            .peer()
            .map_or_else(String::new, |p| format!(" pid {}", p.pid));
        let stats = ConsumerStats::new(format!("client {id}{pid} on {camera}"));
        let mut all = self.0.lock();
        all.retain(|c| c.strong_count() > 0);
        all.push(Arc::downgrade(&stats));
        conn.stats = Some(stats);
        crate::metrics::service_client(true);
    }

    pub(super) fn snapshot(&self) -> Vec<ConsumerMetrics> {
        let mut out: Vec<ConsumerMetrics> = self
            .0
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .map(|c| c.snapshot())
            .collect();
        out.sort_by(|a, b| a.label.cmp(&b.label));
        out
    }
}

/// A client left (see [`Clients::add`]).
pub(super) fn client_left(conn: &mut Connection) {
    if conn.stats.take().is_some() {
        crate::metrics::service_client(false);
    }
}

pub(super) fn memfd(bytes: &[u8]) -> std::io::Result<OwnedFd> {
    // SAFETY: memfd_create returns a new descriptor or -1 (checked).
    let fd = unsafe { libc::memfd_create(c"styx-metrics".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just created and is owned here.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    Ok(file.into())
}

/// Answer a metrics request with `metrics` in `format`.
pub(super) fn answer(conn: &Connection, format: MetricsFormat, metrics: &ServiceMetrics) {
    let body = match format {
        MetricsFormat::Prometheus => metrics.prometheus_text(),
        #[cfg(feature = "metrics-serde")]
        MetricsFormat::Json => match serde_json::to_string(metrics) {
            Ok(json) => json,
            Err(err) => {
                let _ = conn.send(&wire::encode_reject(&format!("metrics: {err}")));
                return;
            }
        },
        #[cfg(not(feature = "metrics-serde"))]
        MetricsFormat::Json => {
            let _ = conn.send(&wire::encode_reject(
                "this camera service was built without metrics as JSON (feature metrics-serde)",
            ));
            return;
        }
    };
    match memfd(body.as_bytes()) {
        Ok(fd) => {
            let _ = conn.send_with_fds(
                &wire::encode_metrics_reply(format, body.len()),
                &[fd.as_raw_fd()],
            );
        }
        Err(err) => {
            let _ = conn.send(&wire::encode_reject(&format!("metrics: {err}")));
        }
    }
}

/// Ask the camera service at `path` for its metrics in `format`.
fn fetch(path: &Path, format: MetricsFormat) -> Result<String, IpcError> {
    let conn = socket::connect(path)?;
    socket::send(&conn, &wire::encode_metrics_request(format), &[])?;
    let deadline = Instant::now() + METRICS_TIMEOUT;
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err(IpcError::Io(std::io::ErrorKind::TimedOut.into()));
        }
        match socket::recv(&conn, wait)? {
            socket::Received::Message(bytes, mut fds) => match wire::decode_server(&bytes)? {
                ServerMessage::Metrics(got, len) => {
                    if got != format || len > MAX_METRICS || fds.is_empty() {
                        return Err(IpcError::Malformed("metrics answer without its memfd"));
                    }
                    let file = std::fs::File::from(fds.swap_remove(0));
                    let mut body = vec![0u8; len];
                    file.read_exact_at(&mut body, 0)?;
                    return String::from_utf8(body)
                        .map_err(|_| IpcError::Malformed("metrics are not UTF-8"));
                }
                ServerMessage::Reject(reason) => return Err(IpcError::Rejected(reason)),
                _ => return Err(IpcError::Malformed("expected metrics")),
            },
            socket::Received::Nothing => {}
            socket::Received::Closed => {
                return Err(IpcError::Io(std::io::ErrorKind::ConnectionReset.into()));
            }
        }
    }
}

impl FrameClient {
    /// The metrics of the [`CameraService`](super::CameraService) at `path` as Prometheus text:
    /// its counters, each client (frames sent, dropped, held, hold times) and every capture of
    /// its process (see [`crate::metrics`]).
    pub fn service_metrics_text(path: impl AsRef<Path>) -> Result<String, IpcError> {
        fetch(path.as_ref(), MetricsFormat::Prometheus)
    }

    /// [`FrameClient::service_metrics_text`] as data.
    #[cfg(feature = "metrics-serde")]
    pub fn service_metrics(path: impl AsRef<Path>) -> Result<ServiceMetrics, IpcError> {
        let json = fetch(path.as_ref(), MetricsFormat::Json)?;
        serde_json::from_str(&json).map_err(|_| IpcError::Malformed("metrics JSON"))
    }
}
