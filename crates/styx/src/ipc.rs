//! Frames for other processes on the same machine.
//!
//! [`FrameServer`] publishes frames on a Unix socket and [`FrameClient`] receives them in another
//! process. Frames in dma-bufs (libcamera, V4L2) and memfds are passed as their file descriptors,
//! without copying; other frames (decoded on the heap) are copied once into a memfd that every
//! client shares. The server keeps a frame's buffers until each client that received it has
//! dropped it, and skips a client that already holds `max_in_flight` frames, so a slow client
//! never holds up the camera or the other clients.
//!
//! Frames arrive with their format, timestamp, clock and crop; companions (pyramid levels) are
//! not sent.

mod socket;
mod wire;

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use styx_core::prelude::*;

use self::wire::{FrameHeader, WireBacking};

/// Frames a client may hold before the server skips it (see [`FrameServer::max_in_flight`]).
pub const DEFAULT_MAX_IN_FLIGHT: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("socket error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame cannot be shared: {0}")]
    Export(#[from] FrameExportError),
    #[error("malformed message: {0}")]
    Malformed(&'static str),
}

/// Counters of a [`FrameServer`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameServerStats {
    /// Frames passed to [`FrameServer::publish`].
    pub published: u64,
    /// Frames sent, counted once per client.
    pub sent: u64,
    /// Published frames that had to be copied into a memfd (not in shareable memory).
    pub copied: u64,
    /// Frames a client did not get because it held `max_in_flight` frames or was not reading.
    pub skipped: u64,
    /// Clients connected now.
    pub clients: usize,
}

struct Client {
    socket: OwnedFd,
    /// Frames sent and not yet released, with the buffers they keep alive.
    held: HashMap<u64, Option<Arc<dyn ExternalBacking>>>,
}

struct ServerState {
    clients: Vec<Client>,
    next_id: u64,
    stats: FrameServerStats,
}

/// Publishes frames to [`FrameClient`]s in other processes over a Unix socket.
pub struct FrameServer {
    listener: OwnedFd,
    path: PathBuf,
    max_in_flight: usize,
    state: Mutex<ServerState>,
}

impl FrameServer {
    /// Listen on the Unix socket at `path`, replacing a stale socket file there.
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let path = path.as_ref().to_path_buf();
        let listener = socket::listen(&path)?;
        Ok(Self {
            listener,
            path,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            state: Mutex::new(ServerState {
                clients: Vec::new(),
                next_id: 0,
                stats: FrameServerStats::default(),
            }),
        })
    }

    /// Frames a client may hold at once (default [`DEFAULT_MAX_IN_FLIGHT`]); it gets no new
    /// frames until it drops one. Each held frame may hold a camera buffer.
    pub fn max_in_flight(mut self, frames: usize) -> Self {
        self.max_in_flight = frames.max(1);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stats(&self) -> FrameServerStats {
        let state = self.state.lock();
        FrameServerStats {
            clients: state.clients.len(),
            ..state.stats
        }
    }

    /// Send `frame` to every connected client that can take it; returns how many got it.
    /// Never blocks on a client.
    pub fn publish(&self, frame: &FrameLease) -> Result<usize, IpcError> {
        let mut state = self.state.lock();
        state.stats.published += 1;
        while let Some(socket) = socket::accept(&self.listener)? {
            state.clients.push(Client {
                socket,
                held: HashMap::new(),
            });
        }
        // Releases, and clients that went away.
        state.clients.retain_mut(|client| {
            loop {
                match socket::recv(&client.socket, Duration::ZERO) {
                    Ok(socket::Received::Message(bytes, _)) => {
                        if let Ok(id) = wire::decode_release(&bytes) {
                            client.held.remove(&id);
                        }
                    }
                    Ok(socket::Received::Nothing) => break true,
                    Ok(socket::Received::Closed) | Err(_) => break false,
                }
            }
        });
        let ready: Vec<usize> = (0..state.clients.len())
            .filter(|&i| state.clients[i].held.len() < self.max_in_flight)
            .collect();
        state.stats.skipped += (state.clients.len() - ready.len()) as u64;
        if ready.is_empty() {
            return Ok(0);
        }

        let (backing, keep) = match frame.export_backing() {
            Ok(backing) => (backing, frame.external_backing_handle()),
            Err(FrameExportError::NotExportable) => {
                state.stats.copied += 1;
                (frame.export_or_copy_memfd()?.1, None)
            }
            Err(err) => return Err(err.into()),
        };
        let id = state.next_id;
        state.next_id += 1;
        let layouts = frame.layouts();
        let (wire_backing, fds): (WireBacking, Vec<OwnedFd>) = match backing {
            FrameBackingExport::Memfd { fd, len } => (WireBacking::Memfd { len }, vec![fd]),
            FrameBackingExport::DmabufPlanes { mut planes } => {
                // A view (e.g. the Y plane of NV12) uses the first planes of its buffer.
                planes.truncate(layouts.len());
                let wire = WireBacking::Dmabuf(planes.iter().map(|p| (p.offset, p.len)).collect());
                (wire, planes.into_iter().map(|p| p.fd).collect())
            }
        };
        let message = wire::encode_frame(&FrameHeader {
            id,
            meta: frame.meta().clone(),
            layouts: layouts.to_vec(),
            backing: wire_backing,
        });
        let raw_fds: Vec<i32> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let (mut sent, mut full) = (0, 0);
        let mut gone = Vec::new();
        for i in ready {
            let client = &mut state.clients[i];
            match socket::send(&client.socket, &message, &raw_fds) {
                Ok(true) => {
                    client.held.insert(id, keep.clone());
                    sent += 1;
                }
                // Its socket is full: it is not reading.
                Ok(false) => full += 1,
                Err(_) => gone.push(i),
            }
        }
        for i in gone.into_iter().rev() {
            state.clients.swap_remove(i);
        }
        state.stats.sent += sent as u64;
        state.stats.skipped += full;
        Ok(sent)
    }
}

impl Drop for FrameServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Receives frames from a [`FrameServer`] in another process.
pub struct FrameClient {
    socket: Arc<OwnedFd>,
}

impl FrameClient {
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        Ok(Self {
            socket: Arc::new(socket::connect(path.as_ref())?),
        })
    }

    /// The next frame, waiting up to `wait`; `Closed` once the server is gone. The server keeps
    /// the frame's buffers until the returned frame is dropped.
    pub fn recv(&self, wait: Duration) -> RecvOutcome<FrameLease> {
        match socket::recv(&self.socket, wait) {
            Ok(socket::Received::Message(bytes, fds)) => match self.frame(&bytes, fds) {
                Ok(frame) => RecvOutcome::Data(frame),
                Err(err) => {
                    tracing::warn!(error = %err, "shared frame skipped");
                    RecvOutcome::Empty
                }
            },
            Ok(socket::Received::Nothing) => RecvOutcome::Empty,
            Ok(socket::Received::Closed) | Err(_) => RecvOutcome::Closed,
        }
    }

    fn frame(&self, bytes: &[u8], fds: Vec<OwnedFd>) -> Result<FrameLease, IpcError> {
        let header = wire::decode_frame(bytes)?;
        let release = Release {
            socket: self.socket.clone(),
            id: header.id,
        };
        let layouts: smallvec::SmallVec<[PlaneLayout; 3]> =
            header.layouts.iter().copied().collect();
        let imported = match header.backing {
            WireBacking::Memfd { .. } => {
                let fd = fds
                    .into_iter()
                    .next()
                    .ok_or(IpcError::Malformed("no memfd"))?;
                FrameLease::from_memfd(header.meta, layouts.clone(), fd)
            }
            WireBacking::Dmabuf(planes) => {
                if fds.len() != planes.len() {
                    return Err(IpcError::Malformed("plane fds missing"));
                }
                let planes = fds
                    .into_iter()
                    .zip(planes)
                    .map(|(fd, (offset, len))| FrameFdPlane { fd, offset, len })
                    .collect();
                FrameLease::from_dmabuf(header.meta, layouts.clone(), planes)?
            }
        };
        let inner = imported
            .external_backing_handle()
            .ok_or(IpcError::Malformed("frame without backing"))?;
        let meta = imported.meta().clone();
        drop(imported);
        Ok(FrameLease::from_external(
            meta,
            layouts,
            Arc::new(Released { inner, release }),
        ))
    }
}

/// Tells the server a frame was dropped, so it can let go of its buffers.
struct Release {
    socket: Arc<OwnedFd>,
    id: u64,
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = socket::send(&self.socket, &wire::encode_release(self.id), &[]);
    }
}

/// A received frame's memory; releases the frame on the server when dropped.
struct Released {
    inner: Arc<dyn ExternalBacking>,
    #[allow(dead_code)] // held for its `Drop`
    release: Release,
}

impl ExternalBacking for Released {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        self.inner.plane_data(index)
    }

    fn backing_bytes(&self) -> Option<usize> {
        self.inner.backing_bytes()
    }

    fn backing_kind(&self) -> &'static str {
        "ipc"
    }

    fn can_export(&self) -> bool {
        self.inner.can_export()
    }

    fn residency(&self) -> FrameResidency {
        self.inner.residency()
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        self.inner.export_backing()
    }
}

#[cfg(test)]
mod tests;
