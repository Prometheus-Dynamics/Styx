//! The consumer side of a frame socket: fetching the latest frame, leased, with its hops.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use styx_core::lease_codec;
use styx_core::metrics::HopCounters;
use styx_core::prelude::*;

use super::super::{IpcError, socket};
use crate::metrics::HopMetrics;

/// Fetch the latest frame from a [`FrameSocket`](super::FrameSocket) (or any
/// `styx-frame-lease-v1` server), waiting up to `wait` for it. The frame is leased: the
/// connection stays open, and the server keeps the frame's buffer, until the returned lease (and
/// every view of it) is dropped. Its hops (`FrameMeta::hops`) end with this process's receive
/// and import times. [`FrameFetcher`] does the same with its buffers kept between fetches and
/// the hop times recorded.
pub fn fetch_frame(path: impl AsRef<Path>, wait: Duration) -> Result<FrameLease, IpcError> {
    FrameFetcher::new(path).fetch(wait)
}

/// Fetches frames from one frame socket again and again: its receive buffers are kept between
/// fetches, and the hop times of the frames it fetched (sensor to import, with the server's
/// hops) are recorded ([`FrameFetcher::hop_metrics`]).
pub struct FrameFetcher {
    path: PathBuf,
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    hops: HopCounters,
}

impl FrameFetcher {
    /// Fetches from the frame socket at `path`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            bytes: Vec::new(),
            fds: Vec::new(),
            hops: HopCounters::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The latest frame (see [`fetch_frame`]); its hops are recorded here.
    pub fn fetch(&mut self, wait: Duration) -> Result<FrameLease, IpcError> {
        let socket = socket::connect_stream(&self.path)?;
        let deadline = Instant::now() + wait;
        self.bytes.clear();
        self.fds.clear();
        const HEADER: usize = lease_codec::HEADER_LEN;
        let payload_len = loop {
            if self.bytes.len() >= HEADER {
                let header = self.bytes[..HEADER].try_into().expect("header");
                let len = lease_codec::payload_len(header)?;
                if self.bytes.len() >= HEADER + len {
                    break len;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match socket::recv_into(&socket, left, &mut self.bytes, &mut self.fds)? {
                socket::Got::Message => {}
                socket::Got::Nothing if left.is_zero() => {
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
                }
                socket::Got::Nothing => {}
                socket::Got::Closed => return Err(IpcError::Malformed("no frame served")),
            }
        };
        let received = CaptureInstant::try_now();
        let payload = &self.bytes[HEADER..HEADER + payload_len];
        let fds = std::mem::take(&mut self.fds);
        let imported = lease_codec::decode(payload, fds)?;
        let inner = imported
            .external_backing_handle()
            .ok_or(IpcError::Malformed("frame without backing"))?;
        let mut meta = imported.meta().clone();
        let layouts = imported.layouts();
        drop(imported);
        if let Some(at) = received {
            meta.hops.set(Hop::Received, at.as_nanos());
        }
        meta.hops.mark(Hop::Imported);
        self.hops.record(&meta.hops);
        Ok(FrameLease::from_external(
            meta,
            layouts,
            Arc::new(Leased {
                inner,
                _connection: socket,
            }),
        ))
    }

    /// Hop times (the server's, then receive and import here) and copies of the frames fetched.
    pub fn hop_metrics(&self) -> HopMetrics {
        HopMetrics::of(&self.hops)
    }
}

/// Import `payload` as a frame message over `fds` (memfds standing in for what a server
/// sends) with [`lease_codec::decode`] and read every plane, as a consumer would. For fuzzing.
#[doc(hidden)]
pub fn fuzz_import(payload: &[u8], fds: Vec<OwnedFd>) {
    if let Ok(frame) = lease_codec::decode(payload, fds) {
        let _ = frame.validate_plane_layouts();
        let _ = std::hint::black_box(frame.meta().hop_record());
        if let Ok(planes) = frame.planes_visible() {
            for rows in planes {
                for row in rows {
                    std::hint::black_box(row.data().iter().fold(0u8, |a, b| a ^ b));
                }
            }
        }
        let _ = std::hint::black_box(frame.to_visible_vec());
    }
}

/// A fetched frame's memory and the connection that is its lease.
struct Leased {
    inner: Arc<dyn ExternalBacking>,
    _connection: OwnedFd,
}

impl ExternalBacking for Leased {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.inner.plane_data(index)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.inner.backing_bytes()
    }

    fn backing_kind(&self) -> &'static str {
        "frame_socket"
    }

    fn can_export(&self) -> bool {
        self.inner.can_export()
    }

    fn residency(&self) -> FrameResidency {
        self.inner.residency()
    }

    fn cpu_access(&self) -> CpuAccess {
        self.inner.cpu_access()
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        self.inner.export_backing()
    }
}
