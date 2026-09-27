//! Receiving frames in another process.

use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smallvec::SmallVec;
use styx_core::prelude::*;

use super::wire::{self, ServerMessage, WireBacking, WireFrame};
use super::{IpcError, socket};

/// How long [`FrameClient::request`] waits for the service's answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Receives frames from a [`FrameServer`](super::FrameServer) or a
/// [`CameraService`](super::CameraService) in another process.
pub struct FrameClient {
    socket: Arc<OwnedFd>,
    plan: Option<String>,
    #[cfg(feature = "async")]
    async_fd: std::sync::OnceLock<tokio::io::unix::AsyncFd<OwnedFd>>,
}

impl FrameClient {
    /// Connect to a [`FrameServer`](super::FrameServer).
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        Ok(Self::new(socket::connect(path.as_ref())?, None))
    }

    /// Ask the [`CameraService`](super::CameraService) at `path` for frames that meet
    /// `requirements`. Fails with [`IpcError::Rejected`] (and the planner's reasons) when the
    /// camera cannot serve them next to its other consumers.
    pub fn request(
        path: impl AsRef<Path>,
        requirements: &FrameRequirements,
    ) -> Result<Self, IpcError> {
        let socket = socket::connect(path.as_ref())?;
        socket::send(&socket, &wire::encode_request(requirements), &[])?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return Err(IpcError::Io(std::io::ErrorKind::TimedOut.into()));
            }
            match socket::recv(&socket, wait)? {
                socket::Received::Message(bytes, _) => match wire::decode_server(&bytes)? {
                    ServerMessage::Accept(plan) => return Ok(Self::new(socket, Some(plan))),
                    ServerMessage::Reject(reason) => return Err(IpcError::Rejected(reason)),
                    ServerMessage::Frame(..) => {}
                },
                socket::Received::Nothing => {}
                socket::Received::Closed => {
                    return Err(IpcError::Io(std::io::ErrorKind::ConnectionReset.into()));
                }
            }
        }
    }

    fn new(socket: OwnedFd, plan: Option<String>) -> Self {
        Self {
            socket: Arc::new(socket),
            plan,
            #[cfg(feature = "async")]
            async_fd: std::sync::OnceLock::new(),
        }
    }

    /// The plan a [`CameraService`](super::CameraService) made for this client.
    pub fn plan(&self) -> Option<&str> {
        self.plan.as_deref()
    }

    /// Change the region of interest (full-frame pixels) of a camera service's frames.
    pub fn set_roi(&self, roi: Option<FrameRect>) -> Result<(), IpcError> {
        socket::send(&self.socket, &wire::encode_roi(roi), &[])?;
        Ok(())
    }

    /// The next frame, waiting up to `wait`; `Closed` once the server is gone. The server keeps
    /// the frame's buffers until the returned frame (and its companions) are dropped.
    pub fn recv(&self, wait: Duration) -> RecvOutcome<FrameLease> {
        match socket::recv(&self.socket, wait) {
            Ok(socket::Received::Message(bytes, fds)) => self.frame(&bytes, fds),
            Ok(socket::Received::Nothing) => RecvOutcome::Empty,
            Ok(socket::Received::Closed) | Err(_) => RecvOutcome::Closed,
        }
    }

    /// Await the next frame; `Closed` once the server is gone.
    #[cfg(feature = "async")]
    pub async fn recv_async(&self) -> RecvOutcome<FrameLease> {
        let Ok(fd) = self.async_fd() else {
            return RecvOutcome::Closed;
        };
        loop {
            let Ok(mut ready) = fd.readable().await else {
                return RecvOutcome::Closed;
            };
            match socket::recv(&self.socket, Duration::ZERO) {
                Ok(socket::Received::Message(bytes, fds)) => {
                    if let RecvOutcome::Data(frame) = self.frame(&bytes, fds) {
                        return RecvOutcome::Data(frame);
                    }
                }
                Ok(socket::Received::Nothing) => ready.clear_ready(),
                Ok(socket::Received::Closed) | Err(_) => return RecvOutcome::Closed,
            }
        }
    }

    #[cfg(feature = "async")]
    fn async_fd(&self) -> std::io::Result<&tokio::io::unix::AsyncFd<OwnedFd>> {
        if let Some(fd) = self.async_fd.get() {
            return Ok(fd);
        }
        let fd = tokio::io::unix::AsyncFd::with_interest(
            self.socket.try_clone()?,
            tokio::io::Interest::READABLE,
        )?;
        Ok(self.async_fd.get_or_init(|| fd))
    }

    fn frame(&self, bytes: &[u8], fds: Vec<OwnedFd>) -> RecvOutcome<FrameLease> {
        let imported = wire::decode_server(bytes).and_then(|message| match message {
            ServerMessage::Frame(id, frame) => {
                let release = Arc::new(Release {
                    socket: self.socket.clone(),
                    id,
                });
                import(*frame, &mut fds.into_iter(), &release).map(Some)
            }
            ServerMessage::Accept(_) | ServerMessage::Reject(_) => Ok(None),
        });
        match imported {
            Ok(Some(frame)) => RecvOutcome::Data(frame),
            Ok(None) => RecvOutcome::Empty,
            Err(err) => {
                tracing::warn!(error = %err, "shared frame skipped");
                RecvOutcome::Empty
            }
        }
    }
}

fn import(
    frame: WireFrame,
    fds: &mut impl Iterator<Item = OwnedFd>,
    release: &Arc<Release>,
) -> Result<FrameLease, IpcError> {
    let count = frame.backing.fd_count();
    let mut own: Vec<OwnedFd> = fds.by_ref().take(count).collect();
    if own.len() != count {
        return Err(IpcError::Malformed("descriptors missing"));
    }
    let layouts: SmallVec<[PlaneLayout; 3]> = frame.layouts.iter().copied().collect();
    let imported = match frame.backing {
        WireBacking::Memfd { .. } => {
            FrameLease::from_memfd(frame.meta, layouts.clone(), own.remove(0))
        }
        WireBacking::Dmabuf(planes) => {
            let planes = own
                .into_iter()
                .zip(planes)
                .map(|(fd, (offset, len))| FrameFdPlane { fd, offset, len })
                .collect();
            FrameLease::from_dmabuf(frame.meta, layouts.clone(), planes)?
        }
    };
    let inner = imported
        .external_backing_handle()
        .ok_or(IpcError::Malformed("frame without backing"))?;
    let meta = imported.meta().clone();
    drop(imported);
    let mut out = FrameLease::from_external(
        meta,
        layouts,
        Arc::new(Released {
            inner,
            _release: release.clone(),
        }),
    );
    for (kind, companion) in frame.companions {
        out = out
            .with_companion(kind, import(companion, fds, release)?)
            .map_err(|_| IpcError::Malformed("companion does not match its frame"))?;
    }
    Ok(out)
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

/// A received frame's memory; the frame is released on the server once it and its companions
/// are all dropped.
struct Released {
    inner: Arc<dyn ExternalBacking>,
    _release: Arc<Release>,
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
