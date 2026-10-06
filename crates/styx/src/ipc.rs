//! Frames for other processes on the same machine.
//!
//! - [`CameraService`] serves cameras: each [`FrameClient`] asks a camera for the frames it needs
//!   ([`FrameClient::request`]), and the service plans one shared capture per camera for all of
//!   them, encoding too when clients ask for H.264, H.265 or MJPEG.
//! - [`FrameServer`] publishes frames your own code produces to any clients that connect
//!   ([`FrameClient::connect`]).
//!
//! Clients of a camera service can also set, read and list its camera's controls and follow
//! their changes ([`FrameClient::set_control`], [`ControlPolicy`]), and receive frames without
//! a thread of their own ([`FrameClient::try_next`] on a pollable descriptor,
//! [`FrameClient::next`] on any executor).
//!
//! Frames in dma-bufs (libcamera, V4L2) and memfds are passed as their file descriptors, without
//! copying; other frames are copied once into a memfd. Pyramid levels and other companions travel
//! with their frame. The sender keeps a frame's buffers until the client has dropped it (and its
//! companions), and sends a client no more frames while it holds `max_in_flight`, so a slow
//! client never holds up the camera or the other clients: a camera buffer a client holds is not
//! written again until the client drops the frame. Holds are bounded (`max_hold`, default
//! [`DEFAULT_MAX_HOLD`]): a client that keeps a frame longer is disconnected and its frames are
//! taken back, so a stuck client cannot keep camera buffers for ever. Meanwhile the camera never
//! waits for buffers: when consumers hold all of them, frames are dropped until one comes back.

mod client;
mod connection;
mod controls;
#[cfg(feature = "frame-socket")]
pub mod frame_socket;
mod mapcache;
mod metrics;
mod service;
mod socket;
mod wire;

use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use styx_core::prelude::*;

pub use self::client::{
    AfMode, ClientOptions, ControlEvents, DEFAULT_OPEN_TIMEOUT, FrameClient, FrameStream, NextFrame,
};
use self::connection::Connection;
pub use self::controls::{
    AppliedControl, ControlCaller, ControlDescriptor, ControlEvent, ControlPolicy, ControlRefusal,
    ControlTarget, ControlWriters, SERVICE_FRAME_RATE, StandardControl,
};
#[cfg(feature = "frame-socket")]
pub use self::frame_socket::{
    FRAME_SOCKET_TRANSPORT, FrameFetcher, FrameSocket, FrameSocketMetrics, FrameSocketOptions,
    FrameSocketStats, fetch_frame,
};
pub use self::service::{
    CameraService, CameraServiceHandle, CameraServiceStats, DEFAULT_MAX_CLIENTS,
};
pub use self::socket::PeerCredentials;
pub use self::wire::CameraInfo;
/// The `styx-frame-lease-v1` message codec the frame socket uses (styx-core's
/// `lease_codec`; a consumer that needs nothing else depends on `styx-core-rs` with the
/// `lease-codec` feature).
#[cfg(feature = "frame-socket")]
pub use styx_core::lease_codec;

/// Decides whether a connecting process may be served.
type Authorize = dyn Fn(&PeerCredentials) -> bool + Send + Sync;

/// Frames a client may hold before it gets no more (see [`FrameServer::max_in_flight`]).
pub const DEFAULT_MAX_IN_FLIGHT: usize = 2;

/// How long a client may hold a frame before it is disconnected and the frame taken back (see
/// [`FrameServer::max_hold`]).
pub const DEFAULT_MAX_HOLD: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("socket error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame cannot be shared: {0}")]
    Export(#[from] FrameExportError),
    #[error("malformed message: {0}")]
    Malformed(&'static str),
    /// A frame socket message (`styx-frame-lease-v1`) could not be encoded or decoded.
    #[cfg(feature = "frame-socket")]
    #[error("frame lease message: {0}")]
    Lease(#[from] styx_core::lease_codec::LeaseCodecError),
    /// The camera service cannot serve the requested frames next to its other clients.
    #[error("camera service rejected the request: {0}")]
    Rejected(String),
    /// The camera service refused a control change or read.
    #[error("camera control refused: {0}")]
    ControlRefused(#[from] ControlRefusal),
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
    /// Clients disconnected for holding a frame longer than `max_hold`.
    pub revoked: u64,
}

struct ServerState {
    clients: Vec<Connection>,
    stats: FrameServerStats,
    /// The frame being published (refilled for each).
    exported: connection::Exported,
}

/// Publishes frames to [`FrameClient`]s in other processes over a Unix socket.
pub struct FrameServer {
    listener: std::os::fd::OwnedFd,
    path: PathBuf,
    max_in_flight: usize,
    max_hold: Option<Duration>,
    authorize: Option<Box<Authorize>>,
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
            max_hold: Some(DEFAULT_MAX_HOLD),
            authorize: None,
            state: Mutex::new(ServerState {
                clients: Vec::new(),
                stats: FrameServerStats::default(),
                exported: connection::Exported::default(),
            }),
        })
    }

    /// Frames a client may hold at once (default [`DEFAULT_MAX_IN_FLIGHT`]); it gets no new
    /// frames until it drops one. Each held frame may hold a camera buffer.
    pub fn max_in_flight(mut self, frames: usize) -> Self {
        self.max_in_flight = frames.max(1);
        self
    }

    /// How long a client may hold a frame (default [`DEFAULT_MAX_HOLD`]; `None`: as long as it
    /// likes). A client holding one longer is disconnected and its frames are taken back: their
    /// buffers may be written again, so a consumer that needs a frame for longer copies it.
    pub fn max_hold(mut self, max: Option<Duration>) -> Self {
        self.max_hold = max;
        self
    }

    /// Serve only processes `allow` accepts, given the credentials the kernel reports for them.
    pub fn authorize(
        mut self,
        allow: impl Fn(&PeerCredentials) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.authorize = Some(Box::new(allow));
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
            let allowed = self.authorize.as_ref().is_none_or(|allow| {
                socket::peer_credentials(&socket).is_ok_and(|peer| allow(&peer))
            });
            if allowed {
                state.clients.push(Connection::new(socket));
            }
        }
        // Releases, clients that went away, and clients holding a frame too long.
        let max_hold = self.max_hold;
        let before = state.clients.len();
        state
            .clients
            .retain_mut(|client| client.poll(Duration::ZERO).is_ok());
        let mut revoked = 0;
        state.clients.retain(|client| {
            let keep = !client.overheld(max_hold);
            revoked += u64::from(!keep);
            keep
        });
        if revoked > 0 {
            crate::trace::warn!(
                revoked,
                connected = before,
                "frame server: disconnected clients holding a frame longer than {max_hold:?}"
            );
        }
        state.stats.revoked += revoked;
        let ready: Vec<usize> = (0..state.clients.len())
            .filter(|&i| state.clients[i].in_flight() < self.max_in_flight)
            .collect();
        state.stats.skipped += (state.clients.len() - ready.len()) as u64;
        if ready.is_empty() {
            return Ok(0);
        }
        let state = &mut *state;
        if let Err(err) = connection::export_into(frame, &mut state.exported) {
            state.exported.clear();
            return Err(err);
        }
        if state.exported.copied {
            state.stats.copied += 1;
        }
        let (mut sent, mut full) = (0, 0);
        let mut gone = Vec::new();
        for i in ready {
            match state.clients[i].send_frame(&state.exported) {
                Ok(true) => sent += 1,
                // Its socket is full: it is not reading.
                Ok(false) => full += 1,
                Err(_) => gone.push(i),
            }
        }
        state.exported.clear();
        for i in gone.into_iter().rev() {
            state.clients.swap_remove(i);
        }
        state.stats.sent += sent as u64;
        state.stats.skipped += full;
        Ok(sent as usize)
    }
}

impl Drop for FrameServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Decode `bytes` as every message a service or client reads, as they arrive from other
/// processes; requests must survive being encoded again. For fuzzing.
#[doc(hidden)]
pub fn fuzz_messages(bytes: &[u8]) {
    if let Ok(wire::ClientMessage::Request(requirements, camera)) = wire::decode_client(bytes) {
        let again = wire::encode_request(&requirements, camera.as_deref());
        match wire::decode_client(&again) {
            Ok(wire::ClientMessage::Request(back, back_camera)) => {
                assert_eq!(back, requirements);
                assert_eq!(back_camera, camera);
            }
            _ => panic!("a decoded request did not decode again"),
        }
    }
    if let Ok(wire::ClientMessage::Control(request)) = wire::decode_client(bytes) {
        match wire::decode_client(&wire::encode_control(&request)) {
            Ok(wire::ClientMessage::Control(back)) => {
                // Floats compare by bits (a NaN is the same NaN).
                assert_eq!(format!("{back:?}"), format!("{request:?}"));
            }
            _ => panic!("a decoded control request did not decode again"),
        }
    }
    if let Ok(wire::ServerMessage::ControlReply(seq, reply)) = wire::decode_server(bytes) {
        let again = wire::encode_control_reply(seq, &reply);
        assert!(matches!(
            wire::decode_server(&again),
            Ok(wire::ServerMessage::ControlReply(s, _)) if s == seq
        ));
    }
    if let Ok(list) = wire::decode_control_list(bytes) {
        let again = wire::decode_control_list(&wire::encode_control_list(&list))
            .expect("a decoded control list decodes again");
        assert_eq!(again.len(), list.len());
    }
    let mut frame = wire::WireFrame::empty();
    if let Ok(Some(_)) = wire::decode_frame_into(bytes, &mut frame) {
        std::hint::black_box(frame.meta.hop_record());
    }
}

/// Decode `bytes` as a camera service client's request, check it and plan it on virtual
/// cameras, as the service does before opening a camera. For fuzzing.
#[doc(hidden)]
pub fn fuzz_service_request(bytes: &[u8]) {
    service::fuzz_request(bytes);
}

#[cfg(test)]
mod tests;
