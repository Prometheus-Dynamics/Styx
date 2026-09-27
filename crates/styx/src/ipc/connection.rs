//! Sending frames to one client: their memory exported as descriptors, and kept until the client
//! releases each frame.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use styx_core::prelude::*;

use super::wire::{self, ClientMessage, WireBacking, WireFrame};
use super::{IpcError, socket};

/// A frame ready to send to any number of clients.
pub(super) struct Exported {
    frame: WireFrame,
    fds: Vec<OwnedFd>,
    /// Buffers (e.g. camera buffers) a client's copy of the frame keeps alive.
    keep: Vec<Arc<dyn ExternalBacking>>,
    /// Some of it had to be copied into a memfd.
    pub(super) copied: bool,
}

/// Export `frame` and its companions: shareable memory as its descriptors, the rest copied once
/// into a memfd.
pub(super) fn export(frame: &FrameLease) -> Result<Exported, IpcError> {
    let mut out = Exported {
        frame: WireFrame {
            meta: frame.meta().clone(),
            layouts: Vec::new(),
            backing: WireBacking::Memfd { len: 0 },
            companions: Vec::new(),
        },
        fds: Vec::new(),
        keep: Vec::new(),
        copied: false,
    };
    out.frame = export_part(frame, &mut out, true)?;
    Ok(out)
}

fn export_part(
    frame: &FrameLease,
    out: &mut Exported,
    top_level: bool,
) -> Result<WireFrame, IpcError> {
    let layouts = frame.layouts();
    let backing = match frame.export_backing() {
        Ok(backing) => {
            out.keep.extend(frame.external_backing_handle());
            backing
        }
        Err(FrameExportError::NotExportable) => {
            out.copied = true;
            frame.export_or_copy_memfd()?.1
        }
        Err(err) => return Err(err.into()),
    };
    let backing = match backing {
        FrameBackingExport::Memfd { fd, len } => {
            out.fds.push(fd);
            WireBacking::Memfd { len }
        }
        FrameBackingExport::DmabufPlanes { mut planes } => {
            if planes.len() < layouts.len() {
                return Err(FrameExportError::PlaneCountMismatch {
                    expected: layouts.len(),
                    actual: planes.len(),
                }
                .into());
            }
            // A view (e.g. the Y plane of NV12) uses the first planes of its buffer.
            planes.truncate(layouts.len());
            let wire = WireBacking::Dmabuf(planes.iter().map(|p| (p.offset, p.len)).collect());
            out.fds.extend(planes.into_iter().map(|p| p.fd));
            wire
        }
    };
    let mut companions = Vec::new();
    if top_level {
        for (kind, companion) in frame.companions() {
            companions.push((kind, export_part(companion, out, false)?));
        }
    }
    Ok(WireFrame {
        meta: frame.meta().clone(),
        layouts: layouts.to_vec(),
        backing,
        companions,
    })
}

/// One client's connection.
pub(super) struct Connection {
    socket: OwnedFd,
    /// Frames sent and not yet released, with the buffers they keep alive.
    held: HashMap<u64, Vec<Arc<dyn ExternalBacking>>>,
    next_id: u64,
}

impl Connection {
    pub(super) fn new(socket: OwnedFd) -> Self {
        Self {
            socket,
            held: HashMap::new(),
            next_id: 0,
        }
    }

    /// Frames the client holds (sent and not released).
    pub(super) fn in_flight(&self) -> usize {
        self.held.len()
    }

    /// Send a frame without blocking; `Ok(false)` when the client's socket is full.
    pub(super) fn send_frame(&mut self, frame: &Exported) -> io::Result<bool> {
        let id = self.next_id;
        let raw: Vec<RawFd> = frame.fds.iter().map(AsRawFd::as_raw_fd).collect();
        if !socket::send(&self.socket, &wire::encode_frame(id, &frame.frame), &raw)? {
            return Ok(false);
        }
        self.next_id += 1;
        self.held.insert(id, frame.keep.clone());
        Ok(true)
    }

    pub(super) fn send(&self, message: &[u8]) -> io::Result<bool> {
        socket::send(&self.socket, message, &[])
    }

    /// Messages from the client, waiting up to `wait` for the first; releases are applied here.
    /// `Err` once the client is gone.
    pub(super) fn poll(&mut self, wait: Duration) -> Result<Vec<ClientMessage>, ()> {
        let mut messages = Vec::new();
        let mut wait = wait;
        loop {
            match socket::recv(&self.socket, wait) {
                Ok(socket::Received::Message(bytes, _)) => match wire::decode_client(&bytes) {
                    Ok(ClientMessage::Release(id)) => {
                        self.held.remove(&id);
                    }
                    Ok(message) => messages.push(message),
                    Err(err) => tracing::debug!(error = %err, "client message ignored"),
                },
                Ok(socket::Received::Nothing) => return Ok(messages),
                Ok(socket::Received::Closed) | Err(_) => return Err(()),
            }
            wait = Duration::ZERO;
        }
    }
}
