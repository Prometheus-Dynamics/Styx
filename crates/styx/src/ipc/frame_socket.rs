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
//! - **Each frame once:** a consumer that wants every frame, once, as it is published, asks the
//!   sibling endpoint `<path>.next` ([`next_path`], [`FrameFetcher::fetch_next`]) with the
//!   timestamp of the frame it has: the server answers when there is another one, so the
//!   consumer neither polls nor gets a frame twice. Same message, same lease (see the
//!   [`next`] module). Polling the frame socket itself sends the same frame again until the
//!   next is published ([`FrameSocketStats::repeated`] counts those sends).
//!
//! # Hops and statistics
//!
//! A published frame that carries hops ([`FrameMeta::hops`]: its sensor timestamp, dequeue,
//! ISP and queue times) is sent with them, the send time added ([`Hop::Sent`]), as the
//! message's `"hops"` member (see [`lease_codec`]); [`fetch_frame`] and [`FrameFetcher`] add
//! the consumer's receive and import times ([`Hop::Received`], [`Hop::Imported`]), so the
//! consumer has the frame's whole path with its sequence number as the join key. A frame
//! without hops is sent as before (no `"hops"`).
//!
//! The server counts what it sends ([`FrameSocket::metrics`]): its counters, how long
//! consumers held their leases, the hop times of the frames it sent and their copies. Another
//! process reads them without disturbing the frame socket from the sibling endpoint
//! `<path>.stats` ([`stats_path`]): connect, write `json` or `prometheus` (a line), read the
//! answer to the end ([`fetch_metrics`], [`fetch_metrics_text`]; `socat - UNIX:<path>.stats`
//! with `prometheus` typed works too). The frame socket itself is unchanged: a connection to it
//! is always a frame lease.

mod fetch;
pub mod next;
mod stats;

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_core::lease_codec::{self, EncodedFrame};
use styx_core::metrics::{HopCounters, Ring};
use styx_core::prelude::*;

pub use self::fetch::{FetchStats, FrameFetcher, fetch_frame, fuzz_import};
pub use self::next::next_path;
pub use self::stats::{FrameSocketMetrics, fetch_metrics, fetch_metrics_text, stats_path};
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
    /// Frames sent to consumers (every send, a frame sent twice counted twice).
    pub served: u64,
    /// Distinct published frames sent at least once.
    pub served_frames: u64,
    /// Sends on the frame socket itself of a frame it had sent before: a consumer (or another
    /// one) fetching again before the next frame was published.
    pub repeated: u64,
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

/// A published frame, ready to send any number of times. Kept (empty) after its last
/// consumer let go, to be filled with a later frame without allocating.
struct Served {
    /// Publication order.
    number: u64,
    /// Its descriptor's timestamp: what tells it apart for `<path>.next` consumers.
    timestamp: u64,
    /// Times it was sent.
    sends: std::sync::atomic::AtomicU64,
    encoded: EncodedFrame,
    /// The camera buffer behind the descriptors, kept while anyone may read them.
    keep: Option<Arc<dyn ExternalBacking>>,
}

impl Served {
    /// Lets go of the frame's memory (descriptors and buffer), keeping the room.
    fn release(&mut self) {
        self.encoded.clear();
        self.keep = None;
    }
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
    /// Consumers on `<path>.next` waiting for a frame they have not seen.
    next_waiting: Vec<next::Waiter>,
    /// Frames no longer the latest: released once no lease holds them, then refilled.
    retired: Vec<Arc<Served>>,
    /// Connections to the stats endpoint waiting for their request.
    stats_waiting: Vec<(OwnedFd, Instant)>,
    stats: FrameSocketStats,
    stopping: bool,
}

impl State {
    /// Releases retired frames no lease holds any more (their camera buffers go back).
    fn release_unheld(&mut self) {
        for served in &mut self.retired {
            if let Some(s) = Arc::get_mut(served) {
                s.release();
            }
        }
    }

    /// A frame to fill: a released retired one, else a new one.
    fn spare(&mut self) -> Arc<Served> {
        match self
            .retired
            .iter_mut()
            .position(|s| Arc::get_mut(s).is_some())
        {
            Some(i) => self.retired.swap_remove(i),
            None => Arc::new(Served {
                number: 0,
                timestamp: 0,
                sends: std::sync::atomic::AtomicU64::new(0),
                encoded: EncodedFrame::default(),
                keep: None,
            }),
        }
    }

    fn retire(&mut self, served: Option<Arc<Served>>) {
        if let Some(served) = served {
            self.retired.push(served);
        }
        self.release_unheld();
    }
}

struct Shared {
    listener: OwnedFd,
    stats_listener: Option<OwnedFd>,
    next_listener: Option<OwnedFd>,
    options: FrameSocketOptions,
    state: Mutex<State>,
    wake: UnixStream,
    /// Lease hold times: send to the consumer's close (or the lease's end).
    hold: Ring,
    /// Hop times (up to the send) and copies of the frames sent.
    hops: HopCounters,
    path: PathBuf,
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

    /// Listen on `path` and serve consumers on a background thread until dropped; the
    /// statistics on `<path>.stats` ([`stats_path`]; not served if that cannot be bound).
    pub fn bind_with(
        path: impl AsRef<Path>,
        options: FrameSocketOptions,
    ) -> Result<Self, IpcError> {
        let path = path.as_ref().to_path_buf();
        let listener = socket::listen_stream(&path)?;
        let stats_listener = match socket::listen_stream(&stats_path(&path)) {
            Ok(l) => Some(l),
            Err(err) => {
                crate::trace::warn!(error = %err, "frame socket statistics not served");
                None
            }
        };
        let next_listener = match socket::listen_stream(&next_path(&path)) {
            Ok(l) => Some(l),
            Err(err) => {
                crate::trace::warn!(error = %err, "frame socket next-frame endpoint not served");
                None
            }
        };
        let (wake, wake_rx) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wake_rx.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            listener,
            stats_listener,
            next_listener,
            options,
            state: Mutex::new(State {
                latest: None,
                leases: Vec::new(),
                waiting: Vec::new(),
                next_waiting: Vec::new(),
                retired: Vec::new(),
                stats_waiting: Vec::new(),
                stats: FrameSocketStats::default(),
                stopping: false,
            }),
            wake,
            hold: Ring::new(),
            hops: HopCounters::new(),
            path: path.clone(),
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
    /// is let go once no consumer holds it. Allocates nothing in steady state (the frame's
    /// message and descriptor lists are reused), except what the frame's own export needs.
    pub fn publish(&self, frame: &FrameLease) -> Result<(), IpcError> {
        let mut served = self.shared.state.lock().spare();
        let s = Arc::get_mut(&mut served).expect("a spare frame is not shared");
        let filled = s.encoded.fill(frame);
        if let Err(err) = filled {
            s.release();
            self.shared.state.lock().retire(Some(served));
            return Err(err.into());
        }
        s.keep = frame.external_backing_handle().filter(|b| b.can_export());
        s.timestamp = s.encoded.descriptor.timestamp;
        *s.sends.get_mut() = 0;
        let copied = s.encoded.copied;
        {
            let mut state = self.shared.state.lock();
            Arc::get_mut(&mut served).expect("not shared yet").number = state.stats.published;
            let previous = state.latest.replace(served);
            state.retire(previous);
            state.stats.published += 1;
            state.stats.copied += u64::from(copied);
            if let Some(max) = self.shared.options.max_held_frames {
                limit_held(&mut state, max, &self.shared.hold);
            }
        }
        self.wake();
        Ok(())
    }

    /// Stop offering a frame (e.g. the capture stopped); consumers holding one keep it.
    pub fn clear(&self) {
        let mut state = self.shared.state.lock();
        let latest = state.latest.take();
        state.retire(latest);
    }

    pub fn stats(&self) -> FrameSocketStats {
        stats_of(&self.shared)
    }

    /// Counters, lease hold times, hop times and copies of the frames sent, and this process's
    /// metrics snapshot: what `<path>.stats` answers.
    pub fn metrics(&self) -> FrameSocketMetrics {
        stats::metrics_of(&self.shared)
    }

    fn wake(&self) {
        let _ = (&self.shared.wake).write_all_nonblocking();
    }
}

fn stats_of(shared: &Shared) -> FrameSocketStats {
    let state = shared.state.lock();
    FrameSocketStats {
        leases: state.leases.len(),
        held_frames: held_count(&state),
        ..state.stats
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
        if self.shared.stats_listener.is_some() {
            let _ = std::fs::remove_file(stats_path(&self.path));
        }
        if self.shared.next_listener.is_some() {
            let _ = std::fs::remove_file(next_path(&self.path));
        }
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

/// Frames held for consumers besides the latest (distinct publication numbers).
fn held_count(state: &State) -> usize {
    let latest = state.latest.as_ref().map(|l| l.number);
    let mut count = 0;
    for (i, lease) in state.leases.iter().enumerate() {
        let n = lease.frame.number;
        if Some(n) != latest && !state.leases[..i].iter().any(|l| l.frame.number == n) {
            count += 1;
        }
    }
    count
}

/// Ends the leases on the oldest held frames while more than `max` are held.
fn limit_held(state: &mut State, max: usize, hold: &Ring) {
    let latest = state.latest.as_ref().map(|l| l.number);
    let mut revoked = 0u64;
    while held_count(state) > max {
        let Some(oldest) = state
            .leases
            .iter()
            .map(|l| l.frame.number)
            .filter(|&n| Some(n) != latest)
            .min()
        else {
            break;
        };
        state.leases.retain(|l| {
            let keep = l.frame.number != oldest;
            if !keep {
                hold.push_duration(l.since.elapsed());
                revoked += 1;
            }
            keep
        });
    }
    state.release_unheld();
    state.stats.revoked += revoked;
    if revoked > 0 {
        crate::trace::debug!(
            revoked,
            "frame socket: more than {max} frames held, ended the leases on the oldest"
        );
    }
}

/// Send `frame` on a new connection, its hops with the send time (written into `out`, reused);
/// `None` if the consumer could not take it. The hops are recorded when the frame is new to
/// the consumer: always on `<path>.next` (`next`), else on the frame's first send (a frame
/// sent again is counted in `repeated`, its growing age not taken for a hop time).
fn send(
    socket: OwnedFd,
    frame: &Arc<Served>,
    out: &mut Vec<u8>,
    hops: &HopCounters,
    next: bool,
    stats: &mut FrameSocketStats,
) -> Option<Lease> {
    let mut record = frame.encoded.hops;
    if let Some(r) = &mut record {
        r.sent = CaptureInstant::try_now().map(CaptureInstant::as_nanos);
    }
    frame
        .encoded
        .message(record.as_ref())
        .write_framed(out)
        .ok()?;
    let fds = &frame.encoded.fds;
    let mut raw = [0 as RawFd; lease_codec::MAX_FDS];
    for (r, fd) in raw.iter_mut().zip(fds) {
        *r = fd.as_raw_fd();
    }
    match socket::send(&socket, out, &raw[..fds.len()]) {
        Ok(true) => {
            let first = frame
                .sends
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                == 0;
            stats.served += 1;
            stats.served_frames += u64::from(first);
            stats.repeated += u64::from(!first && !next);
            if let Some(r) = &record
                && (first || next)
            {
                hops.record(&r.hops());
            }
            Some(Lease {
                socket: Some(socket),
                frame: frame.clone(),
                since: Instant::now(),
                closed: None,
            })
        }
        _ => None,
    }
}

/// Where each kind of descriptor sits in the poll list.
const POLL_WAKE: usize = 0;
const POLL_LISTENER: usize = 1;
const POLL_STATS: usize = 2;
const POLL_NEXT: usize = 3;
const POLL_FIRST: usize = 4;

/// The service thread: accepts consumers, sends them the latest frame, ends their leases,
/// answers statistics requests. Its lists are kept across iterations: nothing is allocated
/// per frame once they have grown.
fn serve(shared: &Shared, wake: &UnixStream) {
    let options = shared.options;
    let mut polls: Vec<libc::pollfd> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    let mut stats_ready: Vec<OwnedFd> = Vec::new();
    loop {
        // What to watch, and how long until something expires.
        let timeout = {
            let state = shared.state.lock();
            if state.stopping {
                return;
            }
            let now = Instant::now();
            polls.clear();
            polls.push(socket::pollfd(wake.as_raw_fd()));
            polls.push(socket::pollfd(shared.listener.as_raw_fd()));
            polls.push(socket::pollfd(
                shared
                    .stats_listener
                    .as_ref()
                    .map_or(-1, AsRawFd::as_raw_fd),
            ));
            polls.push(socket::pollfd(
                shared.next_listener.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            ));
            let mut next = now + Duration::from_secs(1);
            for w in &state.next_waiting {
                polls.push(socket::pollfd(w.socket.as_raw_fd()));
                next = next.min(w.since + next::NEXT_WAIT);
            }
            for lease in &state.leases {
                match (&lease.socket, lease.closed) {
                    (Some(s), _) => {
                        polls.push(socket::pollfd(s.as_raw_fd()));
                        if let Some(max) = options.max_hold {
                            next = next.min(lease.since + max);
                        }
                    }
                    (None, Some(closed)) => next = next.min(closed + options.linger),
                    (None, None) => {}
                }
            }
            for (s, since) in &state.stats_waiting {
                polls.push(socket::pollfd(s.as_raw_fd()));
                next = next.min(*since + stats::REQUEST_WAIT);
            }
            for (_, since) in &state.waiting {
                next = next.min(*since + options.first_frame_wait);
            }
            next.saturating_duration_since(now)
        };
        if socket::poll_readable(&mut polls, timeout + Duration::from_millis(1)).is_err() {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let ready = |fd: RawFd| {
            polls[POLL_FIRST..]
                .iter()
                .any(|p| p.fd == fd && p.revents != 0)
        };
        if polls[POLL_WAKE].revents != 0 {
            let mut buf = [0u8; 64];
            while matches!(std::io::Read::read(&mut &*wake, &mut buf), Ok(n) if n > 0) {}
        }
        {
            let mut state = shared.state.lock();
            let state = &mut *state;
            if polls[POLL_LISTENER].revents != 0 {
                while let Ok(Some(socket)) = socket::accept(&shared.listener) {
                    state.waiting.push((socket, Instant::now()));
                }
            }
            if polls[POLL_STATS].revents != 0
                && let Some(listener) = &shared.stats_listener
            {
                while let Ok(Some(socket)) = socket::accept(listener) {
                    state.stats_waiting.push((socket, Instant::now()));
                }
            }
            let now = Instant::now();
            if polls[POLL_NEXT].revents != 0
                && let Some(listener) = &shared.next_listener
            {
                while let Ok(Some(socket)) = socket::accept(listener) {
                    state.next_waiting.push(next::Waiter::new(socket, now));
                }
            }
            // Serve the waiting consumers, or give up on them.
            let latest = state.latest.clone();
            let mut i = 0;
            while i < state.waiting.len() {
                match &latest {
                    Some(frame) => {
                        let (socket, _) = state.waiting.swap_remove(i);
                        match send(
                            socket,
                            frame,
                            &mut out,
                            &shared.hops,
                            false,
                            &mut state.stats,
                        ) {
                            Some(lease) => state.leases.push(lease),
                            None => state.stats.unserved += 1,
                        }
                    }
                    None if now.duration_since(state.waiting[i].1) >= options.first_frame_wait => {
                        state.waiting.swap_remove(i);
                        state.stats.unserved += 1;
                    }
                    None => i += 1,
                }
            }
            // `<path>.next` consumers: read their line, send them a frame they have not seen.
            let mut i = 0;
            while i < state.next_waiting.len() {
                let w = &mut state.next_waiting[i];
                let alive = !ready(w.socket.as_raw_fd()) || w.read();
                let frame = latest.as_ref().filter(|f| w.wants(f.timestamp));
                if alive && let Some(frame) = frame {
                    let w = state.next_waiting.swap_remove(i);
                    let socket = OwnedFd::from(w.socket);
                    match send(
                        socket,
                        frame,
                        &mut out,
                        &shared.hops,
                        true,
                        &mut state.stats,
                    ) {
                        Some(lease) => state.leases.push(lease),
                        None => state.stats.unserved += 1,
                    }
                } else if !alive || now.duration_since(w.since) >= next::NEXT_WAIT {
                    state.next_waiting.swap_remove(i);
                    state.stats.unserved += 1;
                } else {
                    i += 1;
                }
            }
            drop(latest);
            // Consumers that closed (or sent anything: a release) end their lease, after the
            // linger; consumers holding too long lose theirs.
            let mut revoked = 0u64;
            state.leases.retain_mut(|lease| {
                if let Some(socket) = &lease.socket {
                    if ready(socket.as_raw_fd()) {
                        lease.socket = None;
                        lease.closed = Some(now);
                        shared.hold.push_duration(now.duration_since(lease.since));
                    } else if options
                        .max_hold
                        .is_some_and(|max| now.duration_since(lease.since) >= max)
                    {
                        shared.hold.push_duration(now.duration_since(lease.since));
                        revoked += 1;
                        return false;
                    }
                }
                match lease.closed {
                    Some(closed) => now.duration_since(closed) < options.linger,
                    None => true,
                }
            });
            state.release_unheld();
            if revoked > 0 {
                state.stats.revoked += revoked;
                crate::trace::warn!(
                    revoked,
                    max_hold = ?options.max_hold,
                    "frame socket: closed consumers that held a frame too long (their buffers may be reused)"
                );
            }
            // Statistics requests that arrived (answered below, without the lock), or gave up.
            let mut i = 0;
            while i < state.stats_waiting.len() {
                let (socket, since) = &state.stats_waiting[i];
                if ready(socket.as_raw_fd()) {
                    stats_ready.push(state.stats_waiting.swap_remove(i).0);
                } else if now.duration_since(*since) >= stats::REQUEST_WAIT {
                    state.stats_waiting.swap_remove(i);
                } else {
                    i += 1;
                }
            }
        }
        for socket in stats_ready.drain(..) {
            stats::answer(shared, socket);
        }
    }
}

#[cfg(test)]
#[path = "frame_socket_tests.rs"]
mod tests;
