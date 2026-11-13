//! The node: a PipeWire `Video/Source` stream whose buffers are a producer's memfds. The node
//! outlives its producer. When the producer goes away the stream stays up and idle, the link backs
//! off (250 ms doubling to 5 s), and when a producer returns the node learns its buffers and
//! connects the stream again with them. The PipeWire loop calls back into [`Pump`] every 2 ms.
//!
//! The caller holds a [`Control`]: it starts the run on one thread, and from any thread reads
//! [`Control::status`] and calls [`Control::stop`].

#[cfg(has_pipewire)]
use std::ffi::{CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(has_pipewire)]
use std::time::{Duration, Instant};

#[cfg(has_pipewire)]
use styx_socket::Frame;

#[cfg(has_pipewire)]
use crate::learn::Buffers;
#[cfg(has_pipewire)]
use crate::link::Link;
use crate::link::LinkStatus;

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    /// The node's name, as PipeWire clients see it (`target-object` in `pipewiresrc`).
    pub name: String,
    /// The producer's socket. The node connects, learns and reconnects to it as it comes and goes.
    pub socket: PathBuf,
    /// Frame rate offered to consumers, numerator and denominator.
    pub fps: (u32, u32),
}

/// Counts for one node. `streams` is how many times the PipeWire stream was connected with a
/// producer's buffers: once for the first producer, and once for each producer that returns with
/// buffers of its own. `frames` is every frame taken from the producer; `busy` of those were
/// dropped because a consumer still held their buffer, and the rest were queued to PipeWire.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NodeStats {
    pub streams: u64,
    pub frames: u64,
    pub busy: u64,
    /// Frames PipeWire gave back.
    pub released: u64,
    /// Frames in an index the node did not learn (the producer's ring has more buffers).
    pub unknown: u64,
}

/// A snapshot of a node: its link to the producer, and its counts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    pub link: LinkStatus,
    pub stats: NodeStats,
}

/// Why a run failed. A producer that goes away is not a failure: the node waits for it.
#[derive(Debug)]
pub enum NodeError {
    /// This build has no PipeWire node: the `pipewire` feature was off, or libpipewire-0.3 was
    /// not found when the crate was built.
    NotBuilt,
    /// The node name has a NUL byte.
    BadConfig(&'static str),
    /// The PipeWire shim could not start, or could not take a producer's new buffers (code from
    /// the shim: -1 memory or loop, -2 format, -3 connect, -4 a buffer could not be used).
    Start(i32),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBuilt => f.write_str("this build has no PipeWire node (feature `pipewire`)"),
            Self::BadConfig(why) => write!(f, "configuration: {why}"),
            Self::Start(code) => write!(f, "the PipeWire node failed (code {code})"),
        }
    }
}

impl std::error::Error for NodeError {}

/// The caller's handle on a run: stop it, and read its status. Shared between the thread that
/// runs the node and the caller's.
#[derive(Debug, Default)]
pub struct Control {
    stop: AtomicBool,
    status: Mutex<NodeStatus>,
}

impl Control {
    /// A handle for one run.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the run to end. The node returns from [`run`] on its next tick.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Whether [`Control::stop`] was called.
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// The node's status as of its last tick.
    pub fn status(&self) -> NodeStatus {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NodeStatus> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(has_pipewire)]
    fn publish(&self, link: &LinkStatus, stats: NodeStats) {
        let mut status = self.lock();
        status.link.clone_from(link);
        status.stats = stats;
    }
}

/// Runs the node until `control` is stopped. The producer is connected and its buffers learned
/// first; the node then streams until stopped, reconnecting whenever the producer goes away.
///
/// # Errors
///
/// [`NodeError`]. A stop before the first producer is learned returns `Ok` with no frames.
pub fn run(config: &NodeConfig, control: &Control) -> Result<NodeStats, NodeError> {
    #[cfg(has_pipewire)]
    {
        run_built(config, control)
    }
    #[cfg(not(has_pipewire))]
    {
        let _ = (config, control);
        Err(NodeError::NotBuilt)
    }
}

/// Whether this build has the PipeWire node (the feature is on and the library was found).
pub const fn node_built() -> bool {
    cfg!(has_pipewire)
}

/// The shim's configuration. Mirrors `styx_pw_config` in `c_api/styx_pipewire.c`.
#[cfg(has_pipewire)]
#[repr(C)]
struct Config {
    name: *const c_char,
    fourcc: u32,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    buffers: u32,
    stride: u32,
    fds: *const c_int,
    lens: *const u32,
}

/// One frame the shim is handed. Mirrors `styx_pw_frame`.
#[cfg(has_pipewire)]
#[repr(C)]
struct PwFrame {
    index: u32,
    len: u32,
    stride: u32,
    token: *mut c_void,
}

/// The shim's functions. Mirrors `styx_pw_ops`.
#[cfg(has_pipewire)]
#[repr(C)]
struct Ops {
    poll: unsafe extern "C" fn(*mut c_void, *mut Config) -> c_int,
    next: unsafe extern "C" fn(*mut c_void, *mut PwFrame) -> c_int,
    release: unsafe extern "C" fn(*mut c_void, *mut c_void, c_int),
    running: unsafe extern "C" fn(*mut c_void) -> c_int,
}

#[cfg(has_pipewire)]
unsafe extern "C" {
    fn styx_pw_run(config: *const Config, ops: *const Ops, ctx: *mut c_void) -> c_int;
    fn styx_pw_format_known(fourcc: u32) -> c_int;
}

/// How often the PipeWire loop's timer ticks, and so how often the link is driven.
#[cfg(has_pipewire)]
const TICK: Duration = Duration::from_millis(2);

/// The Rust side of one run: the link, the frames the shim holds (one slot per frame), and the
/// counts.
#[cfg(has_pipewire)]
struct Pump<'a> {
    control: &'a Control,
    name: CString,
    fps: (u32, u32),
    link: Link,
    slots: Vec<Option<Frame>>,
    stats: NodeStats,
    /// The buffers handed to the shim in the call in progress. The shim copies what it keeps, so
    /// they are dropped at the next call.
    handover: Option<Buffers>,
}

#[cfg(has_pipewire)]
impl Pump<'_> {
    /// Drives the link at `now`. Buffers come back only when the producer's format has a PipeWire
    /// name; any other format is refused and the link waits, as for a lost producer.
    fn take(&mut self, now: Instant) -> Option<Buffers> {
        let buffers = self.link.drive(now)?;
        // SAFETY: a plain function of an integer with no pointers.
        if unsafe { styx_pw_format_known(buffers.fourcc.to_u32()) } == 0 {
            let why = format!("format {:?} has no PipeWire video name", buffers.fourcc);
            self.link.reject(now, why);
            return None;
        }
        Some(buffers)
    }

    fn publish(&self) {
        self.control.publish(self.link.status(), self.stats);
    }

    /// Writes the buffers into `out` for the shim, which copies them. Dropping the old frames
    /// first: the consumers' buffers of the old producer are gone with the stream.
    fn poll(&mut self, out: *mut Config) -> bool {
        self.handover = None;
        let Some(buffers) = self.take(Instant::now()) else {
            self.publish();
            return false;
        };
        self.slots = (0..buffers.fds.len()).map(|_| None).collect();
        self.stats.streams += 1;
        let config = self.config_of(&buffers);
        self.handover = Some(buffers);
        // SAFETY: `out` is the shim's config slot, valid for a write; the shim calls `poll` on
        // the thread that runs the loop, and copies the config before this call returns.
        unsafe { out.write(config) };
        self.publish();
        true
    }

    /// The shim's config for `buffers`. The pointers stay valid while `buffers` is held.
    fn config_of(&self, buffers: &Buffers) -> Config {
        Config {
            name: self.name.as_ptr(),
            fourcc: buffers.fourcc.to_u32(),
            width: buffers.resolution.width.get(),
            height: buffers.resolution.height.get(),
            fps_num: self.fps.0,
            fps_den: self.fps.1,
            buffers: buffers.fds.len() as u32,
            stride: buffers.stride,
            fds: buffers.raw_fds().as_ptr(),
            lens: buffers.lens.as_ptr(),
        }
    }

    /// The next frame, held in a free slot, or `None` (nothing new, no slot free, or the producer
    /// is not streaming).
    fn next(&mut self) -> Option<PwFrame> {
        let slot = self.slots.iter().position(Option::is_none)?;
        let frame = self.link.next_frame(Instant::now())?;
        let index = frame.buffer();
        if index >= self.slots.len() {
            self.stats.unknown += 1;
            return None;
        }
        let lease = frame.lease();
        let stride = lease
            .layouts()
            .first()
            .map_or(0, |plane| plane.stride as u32);
        let len = lease.layouts().span().unwrap_or(0) as u32;
        self.stats.frames += 1;
        self.slots[slot] = Some(frame);
        Some(PwFrame {
            index: index as u32,
            len,
            stride,
            token: (slot + 1) as *mut c_void,
        })
    }

    /// Gives back a frame the shim held: PipeWire returned its buffer, or the shim dropped it.
    fn release(&mut self, token: *mut c_void, busy: bool) {
        let slot = (token as usize).wrapping_sub(1);
        if let Some(entry) = self.slots.get_mut(slot) {
            *entry = None;
        }
        if busy {
            self.stats.busy += 1;
        } else {
            self.stats.released += 1;
        }
    }
}

#[cfg(has_pipewire)]
fn run_built(config: &NodeConfig, control: &Control) -> Result<NodeStats, NodeError> {
    let name = CString::new(config.name.as_str())
        .map_err(|_| NodeError::BadConfig("the node name has a NUL byte"))?;
    let fps = (config.fps.0, config.fps.1.max(1));
    let mut pump = Pump {
        control,
        name,
        fps,
        link: Link::new(&config.socket),
        slots: Vec::new(),
        stats: NodeStats::default(),
        handover: None,
    };
    // The node starts once a producer has taught it its buffers. Until then it only waits.
    let first = loop {
        if control.stopped() {
            pump.link.stop();
            pump.publish();
            return Ok(pump.stats);
        }
        if let Some(buffers) = pump.take(Instant::now()) {
            break buffers;
        }
        pump.publish();
        std::thread::sleep(TICK);
    };
    pump.slots = (0..first.fds.len()).map(|_| None).collect();
    pump.stats.streams += 1;
    let shim = pump.config_of(&first);
    let ops = Ops {
        poll: poll_link,
        next: next_frame,
        release: release_frame,
        running,
    };
    // SAFETY: `shim` points into `first`, which lives until the call returns, and the shim copies
    // the descriptors and lengths it keeps. `ops` is a static table of functions that take `ctx`,
    // which is `pump` and lives for the call; the shim calls them only on the thread that runs it,
    // inside the call.
    let rc = unsafe { styx_pw_run(&raw const shim, &raw const ops, (&raw mut pump).cast()) };
    pump.link.stop();
    pump.publish();
    if rc != 0 {
        return Err(NodeError::Start(rc));
    }
    Ok(pump.stats)
}

#[cfg(has_pipewire)]
unsafe extern "C" fn poll_link(ctx: *mut c_void, out: *mut Config) -> c_int {
    // SAFETY: `ctx` is the `Pump` `run_built` passed to the shim for this call, and the shim only
    // calls from the thread that runs the call, so this is the only reference.
    let pump = unsafe { &mut *ctx.cast::<Pump<'_>>() };
    i32::from(pump.poll(out))
}

#[cfg(has_pipewire)]
unsafe extern "C" fn next_frame(ctx: *mut c_void, out: *mut PwFrame) -> c_int {
    // SAFETY: as in `poll_link`.
    let pump = unsafe { &mut *ctx.cast::<Pump<'_>>() };
    match pump.next() {
        Some(frame) => {
            // SAFETY: `out` is the shim's frame slot, valid for a write.
            unsafe { out.write(frame) };
            1
        }
        None => 0,
    }
}

#[cfg(has_pipewire)]
unsafe extern "C" fn release_frame(ctx: *mut c_void, token: *mut c_void, busy: c_int) {
    // SAFETY: as in `poll_link`.
    let pump = unsafe { &mut *ctx.cast::<Pump<'_>>() };
    pump.release(token, busy != 0);
}

#[cfg(has_pipewire)]
unsafe extern "C" fn running(ctx: *mut c_void) -> c_int {
    // SAFETY: as in `poll_link`.
    let pump = unsafe { &*ctx.cast::<Pump<'_>>() };
    i32::from(!pump.control.stopped())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::LinkState;

    fn config() -> NodeConfig {
        NodeConfig {
            name: "styx-test".into(),
            socket: PathBuf::from("/nonexistent/styx-pw-node-test.sock"),
            fps: (30, 1),
        }
    }

    #[test]
    fn a_build_without_the_node_says_so() {
        if !node_built() {
            assert!(matches!(
                run(&config(), &Control::new()),
                Err(NodeError::NotBuilt)
            ));
        }
    }

    #[test]
    fn a_stop_before_any_producer_ends_the_run_with_no_frames() {
        let control = Control::new();
        control.stop();
        let result = run(&config(), &control);
        if node_built() {
            assert_eq!(
                result.expect("a stop is not an error"),
                NodeStats::default()
            );
            assert_eq!(control.status().link.state, LinkState::Stopped);
        } else {
            assert!(matches!(result, Err(NodeError::NotBuilt)));
        }
    }
}
