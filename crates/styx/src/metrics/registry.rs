//! Every capture of the process, for [`snapshot`]; and process-wide numbers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Weak};

use super::camera::CameraMetrics;
use super::live::{CaptureMetrics, Live};

static CAPTURES: Mutex<Vec<Weak<Live>>> = Mutex::new(Vec::new());
static SERVICE_CLIENTS: AtomicUsize = AtomicUsize::new(0);

/// The process: its captures, camera service clients, CPU and memory.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct ProcessMetrics {
    pub pid: u32,
    /// Captures running now (each camera a consumer or shared capture opened).
    pub cameras_open: usize,
    /// Clients of camera services this process runs.
    pub service_clients: usize,
    /// User and system CPU time of the whole process, in nanoseconds.
    pub cpu_ns: u64,
    pub rss_bytes: u64,
    pub threads: u64,
    /// Distinct dma-bufs the process has open (camera, ISP and imported buffers).
    pub dmabufs: u64,
    pub dmabuf_bytes: u64,
    /// Copies of frame pixels by site, dma-buf syncs, exhausted pools.
    #[cfg_attr(feature = "metrics-serde", serde(default))]
    pub path: super::path::PathMetrics,
}

/// A camera service's metrics, as [`FrameClient::service_metrics`] returns them.
///
/// [`FrameClient::service_metrics`]: crate::ipc::FrameClient::service_metrics
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct ServiceMetrics {
    /// Clients connected now.
    pub clients: usize,
    /// Requests refused (cannot be served, malformed, service full).
    pub rejected: u64,
    pub unauthorized: u64,
    /// Captures restarted for a client that needed another setup.
    pub restarts: u64,
    /// Frames sent, counted once per client.
    pub sent: u64,
    /// Frames copied into a memfd to be sent.
    pub copied: u64,
    /// Frames a client's socket had no room for.
    pub skipped: u64,
    /// Clients disconnected for holding a frame too long.
    pub revoked: u64,
    /// Each connected client: frames sent to it, dropped for it, held by it.
    pub client_metrics: Vec<super::camera::ConsumerMetrics>,
    /// The service process: its cameras (with their consumers) and itself.
    pub snapshot: MetricsSnapshot,
}

/// Metrics of every capture in the process.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct MetricsSnapshot {
    /// When it was taken, in milliseconds since the Unix epoch.
    pub unix_ms: u64,
    pub process: ProcessMetrics,
    pub cameras: Vec<CameraMetrics>,
    /// Every live multi-camera frame grouper's sync quality (`styx::multicam`).
    #[cfg_attr(feature = "metrics-serde", serde(default))]
    pub sync_groups: Vec<super::sync::SyncGroupMetrics>,
}

#[cfg(feature = "metrics-serde")]
impl MetricsSnapshot {
    /// As JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(feature = "metrics-serde")]
impl ServiceMetrics {
    /// As JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// List `capture` in [`snapshot`] while it lives.
pub(crate) fn register(capture: &CaptureMetrics) {
    if let Ok(mut all) = CAPTURES.lock() {
        all.retain(|w| w.strong_count() > 0);
        if !all
            .iter()
            .any(|w| std::ptr::eq(w.as_ptr(), std::sync::Arc::as_ptr(&capture.0)))
        {
            all.push(std::sync::Arc::downgrade(&capture.0));
        }
    }
}

fn captures() -> Vec<CaptureMetrics> {
    CAPTURES
        .lock()
        .map(|all| {
            all.iter()
                .filter_map(Weak::upgrade)
                .map(CaptureMetrics)
                .collect()
        })
        .unwrap_or_default()
}

/// Camera service clients connected (or leaving).
pub(crate) fn service_client(connected: bool) {
    if connected {
        SERVICE_CLIENTS.fetch_add(1, Ordering::Relaxed);
    } else {
        SERVICE_CLIENTS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Metrics of every running capture in this process (opened through
/// [`CaptureRequest`](crate::capture_api::CaptureRequest), the planner or a camera service), and
/// of the process.
pub fn snapshot() -> MetricsSnapshot {
    let mut cameras: Vec<CameraMetrics> = captures().iter().map(|c| c.snapshot()).collect();
    cameras.sort_by_key(|c| c.id);
    MetricsSnapshot {
        unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        process: process_with(cameras.len()),
        cameras,
        sync_groups: super::sync::sync_groups(),
    }
}

/// Process-wide numbers alone.
pub fn process() -> ProcessMetrics {
    process_with(captures().len())
}

fn process_with(cameras_open: usize) -> ProcessMetrics {
    let mut p = ProcessMetrics {
        pid: std::process::id(),
        cameras_open,
        service_clients: SERVICE_CLIENTS.load(Ordering::Relaxed),
        path: super::path::path(),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
            p.cpu_ns = super::live::ticks_ns(&stat).unwrap_or(0);
        }
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
            let rss: u64 = statm
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            p.rss_bytes = rss * page;
        }
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            p.threads = status
                .lines()
                .find_map(|l| l.strip_prefix("Threads:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
        }
        let (count, bytes) = dmabufs();
        p.dmabufs = count;
        p.dmabuf_bytes = bytes;
    }
    p
}

/// Distinct dma-bufs among the process's file descriptors, and their size.
#[cfg(target_os = "linux")]
fn dmabufs() -> (u64, u64) {
    let Ok(fds) = std::fs::read_dir("/proc/self/fd") else {
        return (0, 0);
    };
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0;
    for fd in fds.flatten() {
        let Ok(target) = std::fs::read_link(fd.path()) else {
            continue;
        };
        let target = target.to_string_lossy();
        if !target.starts_with("/dmabuf") && !target.contains("dmabuf") {
            continue;
        }
        let name = fd.file_name();
        let Ok(info) =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", name.to_string_lossy()))
        else {
            continue;
        };
        let field = |key: &str| {
            info.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        // Without `ino:` (older kernels) each descriptor counts as its own buffer.
        let fd_number: u64 = name.to_string_lossy().parse().unwrap_or(0);
        let key = field("ino:").unwrap_or(u64::MAX - fd_number);
        if seen.insert(key) {
            bytes += field("size:").unwrap_or(0);
        }
    }
    (seen.len() as u64, bytes)
}
