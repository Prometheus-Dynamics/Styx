//! Sending frames to one client: their memory exported as descriptors, and kept until the client
//! releases each frame.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smallvec::SmallVec;
use styx_core::prelude::*;

use super::wire::{self, ClientHops, ClientMessage, WireBacking, WireFrame};
use super::{IpcError, socket};

/// Buffers a frame keeps alive while a client holds it (inline: no allocation per frame).
type Keep = SmallVec<[Arc<dyn ExternalBacking>; 4]>;

/// A frame ready to send to any number of clients; refilled for the next frame by
/// [`export_into`] (its lists keep their room).
pub(super) struct Exported {
    frame: WireFrame,
    fds: Vec<OwnedFd>,
    /// Buffers (e.g. camera buffers) a client's copy of the frame keeps alive.
    keep: Keep,
    /// Some of it had to be copied into a memfd.
    pub(super) copied: bool,
    /// What a backing exported, before it goes into `fds` and the wire frame.
    exported: Vec<FrameFdPlane>,
}

impl Default for Exported {
    fn default() -> Self {
        Self {
            frame: empty_wire_frame(),
            fds: Vec::new(),
            keep: Keep::new(),
            copied: false,
            exported: Vec::new(),
        }
    }
}

impl Exported {
    /// Lets go of the frame's memory (its buffers and descriptors), keeping the room.
    pub(super) fn clear(&mut self) {
        self.fds.clear();
        self.keep.clear();
    }
}

fn empty_wire_frame() -> WireFrame {
    WireFrame::empty()
}

/// Export `frame` and its companions into `out` (replacing what it held): shareable memory as
/// its descriptors, the rest copied once into a memfd.
pub(super) fn export_into(frame: &FrameLease, out: &mut Exported) -> Result<(), IpcError> {
    out.clear();
    out.copied = false;
    let mut wire = std::mem::replace(&mut out.frame, empty_wire_frame());
    let filled = export_part(frame, out, &mut wire, true);
    out.frame = wire;
    filled
}

fn export_part(
    frame: &FrameLease,
    out: &mut Exported,
    wire: &mut WireFrame,
    top_level: bool,
) -> Result<(), IpcError> {
    let layouts = frame.layout_slice();
    out.exported.clear();
    let (kind, copied_here) = match frame.export_backing_into(&mut out.exported) {
        Ok(kind) => {
            out.keep.extend(frame.external_backing_handle());
            (kind, false)
        }
        Err(FrameExportError::NotExportable) => {
            out.exported.clear();
            (frame.export_or_copy_memfd_into(&mut out.exported)?.1, true)
        }
        // The backing cannot hand out a descriptor (e.g. v4l2loopback has no VIDIOC_EXPBUF):
        // send a copy rather than nothing.
        Err(FrameExportError::Fd(err)) if frame.can_read_planes() => {
            crate::trace::debug!(error = %err, "frame not exportable, copying it");
            out.exported.clear();
            let owned = frame.materialize_owned();
            (owned.export_or_copy_memfd_into(&mut out.exported)?.1, true)
        }
        Err(err) => return Err(err.into()),
    };
    out.copied |= copied_here;
    wire.backing = match kind {
        ExportedKind::Memfd => {
            let plane = out
                .exported
                .pop()
                .ok_or(IpcError::Malformed("memfd export without a descriptor"))?;
            out.fds.push(plane.fd);
            WireBacking::Memfd { len: plane.len }
        }
        ExportedKind::DmabufPlanes => {
            if out.exported.len() < layouts.len() {
                return Err(FrameExportError::PlaneCountMismatch {
                    expected: layouts.len(),
                    actual: out.exported.len(),
                }
                .into());
            }
            // A view (e.g. the Y plane of NV12) uses the first planes of its buffer.
            out.exported.truncate(layouts.len());
            let mut spans =
                match std::mem::replace(&mut wire.backing, WireBacking::Memfd { len: 0 }) {
                    WireBacking::Dmabuf(v) => v,
                    WireBacking::Memfd { .. } => Vec::new(),
                };
            spans.clear();
            spans.extend(out.exported.iter().map(|p| (p.offset, p.len)));
            out.fds.extend(out.exported.drain(..).map(|p| p.fd));
            WireBacking::Dmabuf(spans)
        }
    };
    wire.meta.clone_from(frame.meta());
    if copied_here {
        wire.meta.hops.copied(layouts.iter().map(|l| l.len).sum());
    }
    wire.layouts.clear();
    wire.layouts.extend_from_slice(layouts);
    // A copy lands in a memfd: host memory, cached.
    wire.cpu_access = if copied_here {
        CpuAccess::Cached
    } else {
        frame.cpu_access()
    };
    let mut n = 0;
    if top_level {
        for (kind, companion) in frame.companions() {
            if wire.companions.len() == n {
                wire.companions.push((kind, empty_wire_frame()));
            }
            let slot = &mut wire.companions[n];
            slot.0 = kind;
            export_part(companion, out, &mut slot.1, false)?;
            n += 1;
        }
    }
    wire.companions.truncate(n);
    Ok(())
}

/// A frame a client holds: since when, the buffers it keeps alive, its hops up to the send.
struct Held {
    id: u64,
    since: Instant,
    _keep: Keep,
    hops: FrameHops,
}

/// One client's connection. Its lists and buffers are kept between frames: sending a frame and
/// taking its release back allocate nothing once they have grown.
pub(super) struct Connection {
    socket: OwnedFd,
    /// Frames sent and not yet released.
    held: Vec<Held>,
    next_id: u64,
    /// The message being sent, and the one being received, with its descriptors.
    out: Vec<u8>,
    rx: Vec<u8>,
    rx_fds: Vec<OwnedFd>,
    /// This client in the camera service's metrics.
    pub(super) stats: Option<Arc<crate::metrics::ConsumerStats>>,
}

impl Connection {
    pub(super) fn new(socket: OwnedFd) -> Self {
        Self {
            socket,
            held: Vec::new(),
            next_id: 0,
            out: Vec::new(),
            rx: Vec::new(),
            rx_fds: Vec::new(),
            stats: None,
        }
    }

    /// Frames the client holds (sent and not released).
    pub(super) fn in_flight(&self) -> usize {
        self.held.len()
    }

    /// Send a frame without blocking; `Ok(false)` when the client's socket is full. The frame's
    /// hops go with it, the send time added.
    pub(super) fn send_frame(&mut self, frame: &Exported) -> io::Result<bool> {
        let id = self.next_id;
        let fds = &frame.fds;
        let mut raw: SmallVec<[RawFd; 8]> = SmallVec::new();
        raw.extend(fds.iter().map(AsRawFd::as_raw_fd));
        let mut hops = frame.frame.meta.hops;
        hops.mark(Hop::Sent);
        wire::encode_frame_into(id, &frame.frame, &hops, &mut self.out);
        if !socket::send(&self.socket, &self.out, &raw)? {
            return Ok(false);
        }
        self.next_id += 1;
        self.held.push(Held {
            id,
            since: Instant::now(),
            _keep: frame.keep.clone(),
            hops,
        });
        if let Some(stats) = &self.stats {
            stats.sent();
        }
        Ok(true)
    }

    /// Whether the client has held a frame for longer than `max` (`None`: no limit).
    pub(super) fn overheld(&self, max: Option<Duration>) -> bool {
        max.is_some_and(|max| self.held.iter().any(|h| h.since.elapsed() > max))
    }

    pub(super) fn send(&self, message: &[u8]) -> io::Result<bool> {
        socket::send(&self.socket, message, &[])
    }

    /// Send `message` with descriptors attached.
    pub(super) fn send_with_fds(&self, message: &[u8], fds: &[RawFd]) -> io::Result<bool> {
        socket::send(&self.socket, message, fds)
    }

    /// Another descriptor for the client's socket (to send it messages from other threads).
    pub(super) fn try_clone_socket(&self) -> io::Result<OwnedFd> {
        self.socket.try_clone()
    }

    /// The client process, as the kernel reports it.
    pub(super) fn peer(&self) -> Option<socket::PeerCredentials> {
        socket::peer_credentials(&self.socket).ok()
    }

    /// The client released frame `id`: its hold time, and its hops with the client's receive
    /// and import times, recorded.
    fn released(&mut self, id: u64, client: Option<ClientHops>) {
        let Some(i) = self.held.iter().position(|h| h.id == id) else {
            return;
        };
        let held = self.held.swap_remove(i);
        if let Some(stats) = &self.stats {
            stats.released(held.since.elapsed());
            let mut hops = held.hops;
            if let Some(c) = client {
                if let Some(ns) = c.received {
                    hops.set(Hop::Received, ns);
                }
                if let Some(ns) = c.imported {
                    hops.set(Hop::Imported, ns);
                }
            }
            stats.hops.record(&hops);
        }
    }

    /// Messages from the client, waiting up to `wait` for the first; releases are applied here.
    /// `Err` once the client is gone.
    pub(super) fn poll(&mut self, wait: Duration) -> Result<Vec<ClientMessage>, ()> {
        let mut messages = Vec::new();
        let mut wait = wait;
        loop {
            self.rx.clear();
            self.rx_fds.clear();
            match socket::recv_into(&self.socket, wait, &mut self.rx, &mut self.rx_fds) {
                Ok(socket::Got::Message) => match wire::decode_client(&self.rx) {
                    Ok(ClientMessage::Release(id, hops)) => self.released(id, hops),
                    Ok(message) => messages.push(message),
                    Err(err) => crate::trace::debug!(error = %err, "client message ignored"),
                },
                Ok(socket::Got::Nothing) => return Ok(messages),
                Ok(socket::Got::Closed) | Err(_) => return Err(()),
            }
            wait = Duration::ZERO;
        }
    }
}
