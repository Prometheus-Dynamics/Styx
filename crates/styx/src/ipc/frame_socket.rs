//! The latest frame on a Unix socket, leased: the frame-lease transport HeliOS uses between its
//! camera service and its consumers (`styx-frame-lease-v1`), served so a consumer's frame is
//! not written again while the consumer holds it.
//!
//! # Wire format
//!
//! A consumer connects to the `SOCK_STREAM` socket and gets the latest frame as one message:
//! a little-endian `u32` payload length, the JSON payload, and the frame's descriptors attached
//! (`SCM_RIGHTS`) in the same `sendmsg`:
//!
//! ```json
//! {"descriptor": {"width": 1280, "height": 800, "fourcc": "NV12", "timestamp": 123,
//!                 "color": "Srgb", "planes": [{"offset": 0, "len": 1024000, "stride": 1280}, ...]},
//!  "backing": {"kind": "dmabuf_planes", "planes": [{"offset": 0, "len": 1024000}, ...]}}
//! ```
//!
//! (`"backing": {"kind": "memfd", "len": ...}` with one descriptor for frames copied into a
//! memfd.) The message is styx-core's [`lease_codec`] (also
//! [`crate::ipc::lease_codec`]): [`LeaseMessage`](styx_core::lease_codec::LeaseMessage), the
//! descriptor order (memfd: one; dma-buf planes: one per plane, in plane order), the limits
//! (4 descriptors, 64 KiB of JSON), `encode` / `decode`. A consumer that needs nothing else
//! of Styx depends on `styx-core-rs` with the `lease-codec` feature. Providers advertise the
//! socket as `styx-frame-lease+unix://<path>` (`lease_codec::endpoint_uri`).
//!
//! # Leases and flow control
//!
//! - **Held:** the connection is the lease. From the moment the frame is sent, the server
//!   keeps the frame (and so the camera buffer behind its descriptors) while the consumer keeps
//!   the connection open. [`fetch_frame`] keeps it open for as long as the returned
//!   [`FrameLease`] (and every view of it) lives.
//! - **Released** when the consumer closes the connection, or sends any byte on it (the
//!   server then closes it); when the consumer's process exits or dies, the kernel closes its
//!   descriptors, which is the same close. The buffer is kept [`FrameSocketOptions::linger`]
//!   longer (default zero) for consumers that close at once and read afterwards (as
//!   helios-engine did): those are otherwise protected only while their frame is the latest.
//! - **Expiry:** a consumer holding the frame longer than [`FrameSocketOptions::max_hold`]
//!   (default 2 s, [`DEFAULT_MAX_HOLD`](super::DEFAULT_MAX_HOLD); `None`: no limit) loses
//!   the lease: the server closes the connection (the consumer reads end of stream, or a reset
//!   if it had sent unread bytes), counts it in [`FrameSocketStats::revoked`] and lets the
//!   buffer go. The consumer's mapping stays valid (no fault, no signal) but the capture may
//!   write the next frames into the buffer from then on: pixels read after the cutoff may
//!   belong to later frames. A consumer that needs a frame longer copies it within `max_hold`.
//!   [`fetch_frame`]'s caller is not told; a raw consumer sees the closed connection.
//! - **Every buffer held:** the socket never blocks the publisher ([`FrameSocket::publish`]
//!   only swaps the latest frame), and the capture never waits for consumers: with every
//!   camera buffer held, it drops frames until one comes back (the native PiSP path does this,
//!   `PipelineError::OutputsHeld` inside the capture). Meanwhile the latest frame stays what it
//!   was, so consumers fetching then get it again (same `timestamp`). A service that would
//!   rather keep the frame rate than slow consumers' leases caps the frames held for them
//!   ([`FrameSocketOptions::max_held_frames`]): publishing one more ends the leases on the
//!   oldest (counted as revoked, with the expiry's consequences).
//! - **Sizing:** the socket keeps the latest frame (one buffer), each frame some consumer
//!   holds keeps one more (consumers on the same frame share its buffer), and the ISP and the
//!   capture queue need theirs. On a native PiSP give each output
//!   `StyxConfig::native_output_buffers` of at least N + 3, N the distinct frames consumers may
//!   hold at once: at most the number of consumers, and fewer when they fetch within the same
//!   frame period. N consumers each holding a frame across a graph tick need N + 3; the default
//!   6 serves three without a dropped frame (measured on the CM5, `docs/native-stack/pipeline.md`).
//! - **Ordering:** a connection gets one frame, the latest published when it is served (a
//!   consumer that connects before the first frame waits up to
//!   [`FrameSocketOptions::first_frame_wait`], then the connection is closed with nothing
//!   sent). Nothing is queued: frames published between two fetches are never seen, and a
//!   fetch after nothing new was published returns the same frame. The message carries no
//!   sequence number; the descriptor's `timestamp` (the frame's capture timestamp) tells frames
//!   apart: equal means the same frame, later means newer.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_core::lease_codec;
use styx_core::prelude::*;

use super::{IpcError, socket};

/// The transport name helios-peripherals writes in its stream metadata
/// ([`lease_codec::TRANSPORT`]).
pub const FRAME_SOCKET_TRANSPORT: &str = lease_codec::TRANSPORT;

/// How a [`FrameSocket`] treats consumers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSocketOptions {
    /// How long a consumer may hold a frame (keep its connection open) before the server closes
    /// the connection and lets the buffer go (`None`: as long as it likes). Default 2 s.
    pub max_hold: Option<Duration>,
    /// How long a frame stays held after its consumer closed the connection: for consumers
    /// that close at once and read the frame afterwards. Default zero.
    pub linger: Duration,
    /// How long a consumer that connects before the first frame waits for it. Default 1 s.
    pub first_frame_wait: Duration,
    /// Most frames held for consumers besides the latest one; publishing a frame that would
    /// make more held ends the leases on the oldest (their buffers may then be written while
    /// those consumers still read them). `None` (default): no limit, a frame is never taken
    /// from a consumer before `max_hold`, and the capture drops frames instead.
    pub max_held_frames: Option<usize>,
}

impl Default for FrameSocketOptions {
    fn default() -> Self {
        Self {
            max_hold: Some(super::DEFAULT_MAX_HOLD),
            linger: Duration::ZERO,
            first_frame_wait: Duration::from_secs(1),
            max_held_frames: None,
        }
    }
}

/// Counters of a [`FrameSocket`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameSocketStats {
    /// Frames passed to [`FrameSocket::publish`].
    pub published: u64,
    /// Published frames copied into a memfd (not in shareable memory).
    pub copied: u64,
    /// Frames sent to consumers.
    pub served: u64,
    /// Leases open now (consumers holding a frame, or lingering).
    pub leases: usize,
    /// Frames held now for consumers besides the latest one.
    pub held_frames: usize,
    /// Consumers whose lease was ended early: they held a frame longer than `max_hold`, or
    /// on the oldest frame when more than `max_held_frames` were held.
    pub revoked: u64,
    /// Consumers that connected and got no frame (none published in time).
    pub unserved: u64,
}

/// A published frame, ready to send any number of times.
struct Served {
    /// Publication order.
    number: u64,
    message: Vec<u8>,
    fds: Vec<OwnedFd>,
    /// The camera buffer behind the descriptors, kept while anyone may read them.
    _keep: Option<Arc<dyn ExternalBacking>>,
}

struct Lease {
    socket: Option<OwnedFd>,
    frame: Arc<Served>,
    since: Instant,
    /// When the consumer closed the connection (the lease then lingers).
    closed: Option<Instant>,
}

struct State {
    latest: Option<Arc<Served>>,
    leases: Vec<Lease>,
    waiting: Vec<(OwnedFd, Instant)>,
    stats: FrameSocketStats,
    stopping: bool,
}

struct Shared {
    listener: OwnedFd,
    options: FrameSocketOptions,
    state: Mutex<State>,
    wake: UnixStream,
}

/// Serves the latest published frame on a Unix socket in the `styx-frame-lease-v1` format, with
/// a lease per connection (see the [module documentation](self)).
pub struct FrameSocket {
    shared: Arc<Shared>,
    path: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl FrameSocket {
    /// Listen on `path` (replacing a stale socket file there) with the default options.
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        Self::bind_with(path, FrameSocketOptions::default())
    }

    /// Listen on `path` and serve consumers on a background thread until dropped.
    pub fn bind_with(
        path: impl AsRef<Path>,
        options: FrameSocketOptions,
    ) -> Result<Self, IpcError> {
        let path = path.as_ref().to_path_buf();
        let listener = socket::listen_stream(&path)?;
        let (wake, wake_rx) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wake_rx.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            listener,
            options,
            state: Mutex::new(State {
                latest: None,
                leases: Vec::new(),
                waiting: Vec::new(),
                stats: FrameSocketStats::default(),
                stopping: false,
            }),
            wake,
        });
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("styx-frame-socket".into())
                .spawn(move || serve(&shared, &wake_rx))?
        };
        Ok(Self {
            shared,
            path,
            thread: Some(thread),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Make `frame` the latest one: consumers connecting from now on get it. The previous one
    /// is let go once no consumer holds it.
    pub fn publish(&self, frame: &FrameLease) -> Result<(), IpcError> {
        let mut served = export(frame)?;
        let copied = served._keep.is_none();
        {
            let mut state = self.shared.state.lock();
            served.number = state.stats.published;
            state.latest = Some(Arc::new(served));
            state.stats.published += 1;
            state.stats.copied += u64::from(copied);
            if let Some(max) = self.shared.options.max_held_frames {
                limit_held(&mut state, max);
            }
        }
        self.wake();
        Ok(())
    }

    /// Stop offering a frame (e.g. the capture stopped); consumers holding one keep it.
    pub fn clear(&self) {
        self.shared.state.lock().latest = None;
    }

    pub fn stats(&self) -> FrameSocketStats {
        let state = self.shared.state.lock();
        FrameSocketStats {
            leases: state.leases.len(),
            held_frames: held_frames(&state).len(),
            ..state.stats
        }
    }

    fn wake(&self) {
        let _ = (&self.shared.wake).write_all_nonblocking();
    }
}

impl Drop for FrameSocket {
    fn drop(&mut self) {
        self.shared.state.lock().stopping = true;
        self.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

trait WakeExt {
    fn write_all_nonblocking(self) -> std::io::Result<()>;
}

impl WakeExt for &UnixStream {
    fn write_all_nonblocking(mut self) -> std::io::Result<()> {
        use std::io::Write;
        // A full wake socket already has a wake-up pending.
        match self.write(&[1]) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            r => r.map(|_| ()),
        }
    }
}

/// The frame as a message and its descriptors; dma-bufs are passed as they are (and their
/// buffer kept), other memory copied once into a memfd.
fn export(frame: &FrameLease) -> Result<Served, IpcError> {
    let keep = frame.external_backing_handle().filter(|b| b.can_export());
    let (message, fds) = lease_codec::encode_framed(frame)?;
    Ok(Served {
        number: 0,
        message,
        fds,
        _keep: keep,
    })
}

/// Frames held for consumers besides the latest, oldest first.
fn held_frames(state: &State) -> Vec<u64> {
    let latest = state.latest.as_ref().map(|l| l.number);
    let mut held: Vec<u64> = state
        .leases
        .iter()
        .map(|l| l.frame.number)
        .filter(|&n| Some(n) != latest)
        .collect();
    held.sort_unstable();
    held.dedup();
    held
}

/// Ends the leases on the oldest held frames while more than `max` are held.
fn limit_held(state: &mut State, max: usize) {
    let held = held_frames(state);
    if held.len() <= max {
        return;
    }
    let oldest = held[held.len() - max - 1];
    let before = state.leases.len();
    state.leases.retain(|l| l.frame.number > oldest);
    let revoked = (before - state.leases.len()) as u64;
    state.stats.revoked += revoked;
    crate::trace::debug!(
        revoked,
        "frame socket: more than {max} frames held, ended the leases on the oldest"
    );
}

/// Send `frame` on a new connection; `None` if the consumer could not take it.
fn send(socket: OwnedFd, frame: &Arc<Served>) -> Option<Lease> {
    let raw: Vec<RawFd> = frame.fds.iter().map(AsRawFd::as_raw_fd).collect();
    match socket::send(&socket, &frame.message, &raw) {
        Ok(true) => Some(Lease {
            socket: Some(socket),
            frame: frame.clone(),
            since: Instant::now(),
            closed: None,
        }),
        _ => None,
    }
}

/// The service thread: accepts consumers, sends them the latest frame, ends their leases.
fn serve(shared: &Shared, wake: &UnixStream) {
    let options = shared.options;
    loop {
        // What to watch, and how long until something expires.
        let (fds, timeout) = {
            let state = shared.state.lock();
            if state.stopping {
                return;
            }
            let now = Instant::now();
            let mut fds = vec![wake.as_raw_fd(), shared.listener.as_raw_fd()];
            let mut next = now + Duration::from_secs(1);
            for lease in &state.leases {
                match (&lease.socket, lease.closed) {
                    (Some(s), _) => {
                        fds.push(s.as_raw_fd());
                        if let Some(max) = options.max_hold {
                            next = next.min(lease.since + max);
                        }
                    }
                    (None, Some(closed)) => next = next.min(closed + options.linger),
                    (None, None) => {}
                }
            }
            for (_, since) in &state.waiting {
                next = next.min(*since + options.first_frame_wait);
            }
            (fds, next.saturating_duration_since(now))
        };
        let Ok(ready) = socket::poll_readable(&fds, timeout + Duration::from_millis(1)) else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        if ready[0] {
            let mut buf = [0u8; 64];
            while matches!(std::io::Read::read(&mut &*wake, &mut buf), Ok(n) if n > 0) {}
        }
        let readable: Vec<RawFd> = fds[2..]
            .iter()
            .zip(&ready[2..])
            .filter(|&(_, &r)| r)
            .map(|(&fd, _)| fd)
            .collect();
        let mut state = shared.state.lock();
        let state = &mut *state;
        if ready[1] {
            while let Ok(Some(socket)) = socket::accept(&shared.listener) {
                state.waiting.push((socket, Instant::now()));
            }
        }
        let now = Instant::now();
        // Serve the waiting consumers, or give up on them.
        let latest = state.latest.clone();
        for (socket, since) in std::mem::take(&mut state.waiting) {
            match &latest {
                Some(frame) => match send(socket, frame) {
                    Some(lease) => {
                        state.stats.served += 1;
                        state.leases.push(lease);
                    }
                    None => state.stats.unserved += 1,
                },
                None if now.duration_since(since) >= options.first_frame_wait => {
                    state.stats.unserved += 1;
                }
                None => state.waiting.push((socket, since)),
            }
        }
        // Consumers that closed (or sent anything: a release) end their lease, after the
        // linger; consumers holding too long lose theirs.
        let mut revoked = 0u64;
        state.leases.retain_mut(|lease| {
            if let Some(socket) = &lease.socket {
                if readable.contains(&socket.as_raw_fd()) {
                    lease.socket = None;
                    lease.closed = Some(now);
                } else if options
                    .max_hold
                    .is_some_and(|max| now.duration_since(lease.since) >= max)
                {
                    revoked += 1;
                    return false;
                }
            }
            match lease.closed {
                Some(closed) => now.duration_since(closed) < options.linger,
                None => true,
            }
        });
        if revoked > 0 {
            state.stats.revoked += revoked;
            crate::trace::warn!(
                revoked,
                max_hold = ?options.max_hold,
                "frame socket: closed consumers that held a frame too long (their buffers may be reused)"
            );
        }
    }
}

/// Fetch the latest frame from a [`FrameSocket`] (or any `styx-frame-lease-v1` server),
/// waiting up to `wait` for it. The frame is leased: the connection stays open, and the
/// server keeps the frame's buffer, until the returned lease (and every view of it) is dropped.
pub fn fetch_frame(path: impl AsRef<Path>, wait: Duration) -> Result<FrameLease, IpcError> {
    let socket = socket::connect_stream(path.as_ref())?;
    let deadline = Instant::now() + wait;
    let mut bytes = Vec::new();
    let mut fds = Vec::new();
    let payload_len = loop {
        const HEADER: usize = lease_codec::HEADER_LEN;
        if bytes.len() >= HEADER {
            let len = lease_codec::payload_len(bytes[..HEADER].try_into().expect("header"))?;
            if bytes.len() >= HEADER + len {
                break len;
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match socket::recv(&socket, left)? {
            socket::Received::Message(more, more_fds) => {
                bytes.extend_from_slice(&more);
                fds.extend(more_fds);
            }
            socket::Received::Nothing if left.is_zero() => {
                return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
            }
            socket::Received::Nothing => {}
            socket::Received::Closed => return Err(IpcError::Malformed("no frame served")),
        }
    };
    let payload = &bytes[lease_codec::HEADER_LEN..lease_codec::HEADER_LEN + payload_len];
    let imported = lease_codec::decode(payload, fds)?;
    let inner = imported
        .external_backing_handle()
        .ok_or(IpcError::Malformed("frame without backing"))?;
    let meta = imported.meta().clone();
    let layouts = imported.layouts().iter().copied().collect();
    drop(imported);
    Ok(FrameLease::from_external(
        meta,
        layouts,
        Arc::new(Leased {
            inner,
            _connection: socket,
        }),
    ))
}

/// Import `payload` as a frame message over `fds` (memfds standing in for what a server
/// sends) with [`lease_codec::decode`] and read every plane, as a consumer would. For fuzzing.
#[doc(hidden)]
pub fn fuzz_import(payload: &[u8], fds: Vec<OwnedFd>) {
    if let Ok(frame) = lease_codec::decode(payload, fds) {
        let _ = frame.validate_plane_layouts();
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

#[cfg(test)]
#[path = "frame_socket_tests.rs"]
mod tests;
