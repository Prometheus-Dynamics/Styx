//! The node: a PipeWire `Video/Source` stream whose buffers are a producer's memfds. The node
//! outlives its producer. When the producer goes away the stream stays up and idle, the link backs
//! off (250 ms doubling to 5 s), and when a producer returns the node learns its buffers and
//! connects the stream again with them. The stream is pipewire-rs; its loop ticks every 2 ms.
//!
//! # What a consumer sees
//!
//! Every frame a consumer takes is whole, and stays whole while the consumer holds it: the node
//! gives a buffer back to the producer only when no consumer has it or can still take it (the
//! rule and what it rests on are in `hold.rs`).
//!
//! Frames are not promised in order. A consumer that keeps up gets them in order. One that
//! stalls skips frames, and when it runs again it can be handed an older frame after a newer one
//! (a PipeWire link holds up to two sent buffers, read in turn), or a frame it was handed before
//! (when every buffer held a frame it could still take, the node sent the oldest again rather
//! than stop; [`NodeStats::resent`] counts those).
//!
//! Each buffer carries PipeWire's header metadata for a consumer that asks for it
//! (`SPA_META_Header`): `seq` is the frame's id and `pts` its capture time in nanoseconds. That
//! is the only ordering signal there is. A consumer that needs order drops any frame whose `seq`
//! is not above the last one it kept.
//!
//! The caller holds a [`Control`]: it starts the run on one thread, and from any thread reads
//! [`Control::status`] and calls [`Control::stop`].

#[cfg(feature = "pipewire")]
use std::cell::RefCell;
#[cfg(feature = "pipewire")]
use std::ffi::c_void;
#[cfg(feature = "pipewire")]
use std::io::Cursor;
#[cfg(feature = "pipewire")]
use std::os::fd::{IntoRawFd, OwnedFd};
use std::path::PathBuf;
#[cfg(feature = "pipewire")]
use std::ptr;
#[cfg(feature = "pipewire")]
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(feature = "pipewire")]
use std::time::{Duration, Instant};

#[cfg(feature = "pipewire")]
use pipewire::{
    context::ContextRc,
    keys,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        buffer::DataType,
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
        },
        pod::{Object, Pod, Property, Value, serialize::PodSerializer},
        utils::{Direction, Fraction, Id, Rectangle, SpaTypes},
    },
    stream::{Stream, StreamFlags, StreamRc},
    sys as pw_sys,
};

#[cfg(feature = "pipewire")]
use styx_socket::Frame;

#[cfg(feature = "pipewire")]
use crate::hold::{Area, Holds};
#[cfg(feature = "pipewire")]
use crate::learn::Buffers;
#[cfg(feature = "pipewire")]
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
    /// Frames sent to PipeWire again: the frames the node held, each still takeable by a
    /// consumer, left the producer no buffer to write to, so the node sent the oldest again
    /// instead of stopping.
    pub resent: u64,
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
    /// This build has no PipeWire node: the `pipewire` feature was off.
    NotBuilt,
    /// The node name has a NUL byte.
    BadConfig(&'static str),
    /// The node could not start, or could not take a producer's new buffers. The code says which
    /// step failed: -1 memory, the PipeWire loop, context or stream, -2 a format with no PipeWire
    /// video name, -3 the stream connect, -4 a buffer PipeWire could not use.
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
    /// Shared with the handles the PipeWire callbacks keep, which outlive the borrow of `run`.
    state: Arc<ControlState>,
}

#[derive(Debug, Default)]
struct ControlState {
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
        self.state.stop.store(true, Ordering::Release);
    }

    /// Whether [`Control::stop`] was called.
    pub fn stopped(&self) -> bool {
        self.state.stop.load(Ordering::Acquire)
    }

    /// The node's status as of its last tick.
    pub fn status(&self) -> NodeStatus {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NodeStatus> {
        self.state
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Another handle on the same run.
    #[cfg(feature = "pipewire")]
    fn share(&self) -> Control {
        Control {
            state: Arc::clone(&self.state),
        }
    }

    #[cfg(feature = "pipewire")]
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
    #[cfg(feature = "pipewire")]
    {
        run_built(config, control, MIN_BUFFERS)
    }
    #[cfg(not(feature = "pipewire"))]
    {
        let _ = (config, control);
        Err(NodeError::NotBuilt)
    }
}

/// The fewest buffers a producer's ring may have. The node holds up to two frames a consumer can
/// still take (a PipeWire link keeps up to two sent buffers) and the producer keeps one buffer for
/// its newest frame, so a smaller ring leaves nothing to write the next frame to: the node then
/// stays alive only by sending held frames again. Measured with a ring of two and a consumer that
/// stalled six times: the producer found no free buffer for 133 of 172 frames, and 22 of the 30
/// frames the consumer took were repeats.
///
/// Three is the least that works, not what to choose. With four, a consumer that never stalls
/// cost the producer 3 of 64 frames, and one that overran its cycle by two frames six times cost
/// it 6 of 48. A consumer stopped for twelve frames six times cost it 47 of 110, which no ring
/// short of twelve more buffers avoids: while a consumer is stopped, each buffer beyond the three
/// is one more frame the producer can write before it has to drop one.
pub const MIN_BUFFERS: usize = 3;

/// [`run`] for a ring smaller than [`MIN_BUFFERS`]: for the test that shows what such a ring does.
#[doc(hidden)]
pub fn run_with_a_small_ring(
    config: &NodeConfig,
    control: &Control,
) -> Result<NodeStats, NodeError> {
    #[cfg(feature = "pipewire")]
    {
        run_built(config, control, 1)
    }
    #[cfg(not(feature = "pipewire"))]
    {
        let _ = (config, control);
        Err(NodeError::NotBuilt)
    }
}

/// Whether this build has the PipeWire node (the `pipewire` feature is on).
pub const fn node_built() -> bool {
    cfg!(feature = "pipewire")
}

/// Where `cycle` is in PipeWire's clock area (`spa_io_clock`), in bytes: the graph cycle count,
/// whose low bit picks which of a link's two io areas a send is written to. It is read by its
/// offset because the field has this name only from PipeWire 1.2. Before that the same four
/// bytes are padding and a link has one io area: a constant there names one area, which is right.
#[cfg(feature = "pipewire")]
const CLOCK_CYCLE_AT: usize = 148;

/// How many of the node's graph cycles with no send pass before a starved node sends a held frame
/// again: long enough for a replacement to settle and its buffer to be freed first.
#[cfg(feature = "pipewire")]
const RESEND_CYCLES: u64 = 2 * crate::hold::SETTLE_CYCLES + 1;

/// The most buffers a PipeWire stream holds.
#[cfg(feature = "pipewire")]
const MAX_BUFFERS: usize = 64;

/// PipeWire's snapshot of the node's last process in the graph, read whole.
#[cfg(feature = "pipewire")]
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    /// The start time of the cycle it was taken in: which cycle it is.
    now: i64,
    /// The graph clock's position in that cycle.
    ticks: u64,
    /// How many buffers the node has queued that had not gone out by then.
    pending: u64,
}

#[cfg(feature = "pipewire")]
fn snapshot(stream: &Stream) -> Option<Snapshot> {
    let mut time = std::mem::MaybeUninit::<pw_sys::pw_time>::zeroed();
    // SAFETY: `stream` is live and `time` is a whole `pw_time` to write; PipeWire copies at most
    // the size given, under its own sequence lock.
    let rc = unsafe {
        pw_sys::pw_stream_get_time_n(
            stream.as_raw_ptr(),
            time.as_mut_ptr(),
            size_of::<pw_sys::pw_time>(),
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: zeroed, then written by the call above.
    let time = unsafe { time.assume_init() };
    Some(Snapshot {
        now: time.now,
        ticks: time.ticks,
        pending: time.queued,
    })
}

/// A buffer the node has queued and not yet seen go out.
#[cfg(feature = "pipewire")]
#[derive(Debug, Clone, Copy)]
struct InFlight {
    index: usize,
    /// The snapshot read just before it was queued, `None` when there was none.
    before: Option<Snapshot>,
}

/// How often the PipeWire loop's timer ticks, and so how often the link is driven.
#[cfg(feature = "pipewire")]
const TICK: Duration = Duration::from_millis(2);

/// The SPA video format for a Styx FourCc, by its little-endian code. `None` when PipeWire has no
/// raw video name for it, and the node refuses such a producer.
#[cfg(feature = "pipewire")]
fn video_format(fourcc: u32) -> Option<VideoFormat> {
    Some(match fourcc {
        0x5659_5559 => VideoFormat::YUY2,  // YUYV
        0x5956_5955 => VideoFormat::UYVY,  // UYVY
        0x3231_564e => VideoFormat::NV12,  // NV12
        0x3132_564e => VideoFormat::NV21,  // NV21
        0x3032_3449 => VideoFormat::I420,  // I420
        0x3231_5559 => VideoFormat::I420,  // YU12
        0x3231_5659 => VideoFormat::YV12,  // YV12
        0x5945_5247 => VideoFormat::GRAY8, // GREY
        0x3332_4752 => VideoFormat::RGB,   // RG24
        0x3332_4742 => VideoFormat::BGR,   // BG24
        0x4142_4752 => VideoFormat::RGBA,  // RGBA
        0x4152_4742 => VideoFormat::BGRA,  // BGRA
        _ => return None,
    })
}

/// What the stream offers PipeWire consumers: the format, the ring's shape and the rate.
#[cfg(feature = "pipewire")]
#[derive(Debug, Default, Clone, Copy)]
struct Format {
    fourcc: u32,
    width: u32,
    height: u32,
    fps: (u32, u32),
    /// Row stride of plane 0.
    stride: u32,
    /// The ring's buffer count.
    buffers: u32,
    /// The length of the first buffer in bytes (every buffer of a ring is alike).
    size: u32,
}

/// The producer side of the node: the link, the frames the node holds (one slot per ring buffer)
/// and the counts.
#[cfg(feature = "pipewire")]
struct Pump {
    control: Control,
    link: Link,
    /// The frame held in each slot. A slot is taken until PipeWire gives its buffer back, or the
    /// frame is dropped.
    slots: Vec<Option<Frame>>,
    stats: NodeStats,
    /// The fewest buffers a producer's ring may have.
    min_buffers: usize,
}

/// A frame taken from the producer, held in `slot` until it is queued or dropped.
#[cfg(feature = "pipewire")]
struct Pulled {
    /// The frame's id and capture time, for the buffer's header.
    id: u64,
    timestamp_ns: u64,
    index: usize,
    len: u32,
    stride: u32,
    slot: usize,
}

#[cfg(feature = "pipewire")]
impl Pump {
    /// Drives the link at `now`. Buffers come back only when the producer's format has a PipeWire
    /// name and its ring is large enough; any other producer is refused, with the reason in the
    /// link's status, and the link waits, as for a lost producer.
    fn take(&mut self, now: Instant) -> Option<Buffers> {
        let buffers = self.link.drive(now)?;
        if video_format(buffers.fourcc.to_u32()).is_none() {
            let why = format!("format {:?} has no PipeWire video name", buffers.fourcc);
            self.link.reject(now, why);
            return None;
        }
        if buffers.fds.len() < self.min_buffers {
            let why = format!(
                "the producer's ring has {} buffers and the node needs at least {}: it holds up \
                 to two frames a PipeWire consumer can still take and the producer keeps one for \
                 its newest, so a smaller ring leaves no buffer for the next frame",
                buffers.fds.len(),
                self.min_buffers
            );
            self.link.reject(now, why);
            return None;
        }
        Some(buffers)
    }

    fn publish(&self) {
        self.control.publish(self.link.status(), self.stats);
    }

    /// The next frame, held in a free slot, or `None` (nothing new, no slot free, or the producer
    /// is not streaming).
    fn next(&mut self) -> Option<Pulled> {
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
        let (id, timestamp_ns) = (lease.id(), lease.timestamp_ns());
        self.stats.frames += 1;
        self.slots[slot] = Some(frame);
        Some(Pulled {
            id,
            timestamp_ns,
            index,
            len,
            stride,
            slot,
        })
    }

    /// Gives back the frame in `slot`: PipeWire returned its buffer (`busy` false), or the node
    /// dropped it because the consumer still held the buffer (`busy` true).
    fn release(&mut self, slot: usize, busy: bool) {
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

/// The node's state, shared by the stream's callbacks and the loop's timer. Single-threaded: the
/// PipeWire loop calls every callback on the thread that runs it, and no callback is entered while
/// another holds the borrow (the borrow is released around every PipeWire call).
#[cfg(feature = "pipewire")]
struct Ring {
    pump: Pump,
    /// The producer's memfds, the node's own duplicates: ring buffer `i` is `fds[i]`.
    fds: Vec<OwnedFd>,
    lens: Vec<u32>,
    /// Where ring buffer `i` starts in `fds[i]`.
    offsets: Vec<u64>,
    format: Format,
    /// PipeWire's buffer for each ring index, null until PipeWire adds it.
    pw: Vec<*mut pw_sys::pw_buffer>,
    /// The slot of the frame queued in each buffer, `None` when none.
    token: Vec<Option<usize>>,
    /// True when PipeWire has the buffer back and the node may fill and queue it.
    avail: Vec<bool>,
    n_avail: usize,
    /// How many buffers PipeWire has added to this stream connection.
    added: usize,
    /// Set when a buffer could not be used; the run returns -4 when it ends.
    failed: bool,
    /// True while the stream's port has its buffers io area (it is linked). PipeWire's dequeue
    /// reads that area without checking it when the node runs with reliable transport (a
    /// `node.reliable` set from outside), so the node asks for no buffer while it is unset.
    io: bool,
    /// The graph's position area while the stream has one, null otherwise. Read only.
    position: *const pipewire::spa::sys::spa_io_position,
    /// The cycle of the last snapshot seen, and how many different ones were seen: the node's
    /// count of graph cycles, which can only run behind the true one.
    last_now: i64,
    cycles: u64,
    /// Which buffers hold a frame a consumer has, or can still take.
    holds: Holds,
    /// The buffer on its way out: one is sent at a time, so each send's cycle can be read.
    in_flight: Option<InFlight>,
    /// The node's cycle count at its last send.
    last_send: u64,
    /// Each buffer's busy count, null when the buffers carry none.
    busy: Vec<*const pipewire::spa::sys::spa_meta_busy>,
    /// Each buffer's header, where the frame's id and time go, null when the consumer did not
    /// ask for one.
    header: Vec<*mut pipewire::spa::sys::spa_meta_header>,
}

#[cfg(feature = "pipewire")]
impl Ring {
    fn new(pump: Pump, fps: (u32, u32)) -> Self {
        Self {
            pump,
            fds: Vec::new(),
            lens: Vec::new(),
            offsets: Vec::new(),
            format: Format {
                fps,
                ..Format::default()
            },
            pw: Vec::new(),
            token: Vec::new(),
            avail: Vec::new(),
            n_avail: 0,
            added: 0,
            failed: false,
            io: false,
            position: ptr::null(),
            last_now: 0,
            cycles: 0,
            holds: Holds::new(0),
            in_flight: None,
            last_send: 0,
            busy: Vec::new(),
            header: Vec::new(),
        }
    }

    /// Takes the producer's buffers as the ring's own, and sizes the per-buffer state to them.
    /// Fails (-1) for a producer with no buffers, or whose lengths do not match its descriptors.
    fn adopt(&mut self, buffers: Buffers) -> Result<(), i32> {
        let Buffers {
            fds,
            lens,
            offsets,
            fourcc,
            resolution,
            stride,
            ..
        } = buffers;
        let count = fds.len();
        if count == 0 || count > MAX_BUFFERS || lens.len() != count || offsets.len() != count {
            return Err(-1);
        }
        self.format = Format {
            fourcc: fourcc.to_u32(),
            width: resolution.width.get(),
            height: resolution.height.get(),
            fps: self.format.fps,
            stride,
            buffers: count as u32,
            size: lens[0],
        };
        self.fds = fds;
        self.lens = lens;
        self.offsets = offsets;
        self.pw = vec![ptr::null_mut(); count];
        self.token = vec![None; count];
        self.avail = vec![false; count];
        self.holds.clear(count);
        self.in_flight = None;
        self.busy = vec![ptr::null(); count];
        self.header = vec![ptr::null_mut(); count];
        self.n_avail = 0;
        self.added = 0;
        // The frames of the old buffers are dropped here, not released: their consumers are gone
        // with the stream.
        self.pump.slots = (0..count).map(|_| None).collect();
        Ok(())
    }

    /// Clears what the node holds in the stream's buffers without giving the frames back. Used
    /// before the stream is disconnected: the frames were dropped with the old producer.
    fn forget_held(&mut self) {
        self.avail.fill(false);
        self.token.fill(None);
        self.holds.clear(self.token.len());
        self.in_flight = None;
        self.n_avail = 0;
    }

    /// The graph clock now: the current cycle's start time, the low bit of its count, and the
    /// length of a cycle in the clock's position. `None` with no clock, or when a cycle started
    /// while it was read.
    fn clock(&self) -> Option<(u64, u8, u64)> {
        if self.position.is_null() {
            return None;
        }
        // SAFETY: `position` is the area PipeWire gave the stream and has not taken back (it is
        // nulled in `io_changed` when it does), a whole `spa_io_position` whose first field is the
        // 160-byte clock, so the four bytes at `CLOCK_CYCLE_AT` are inside it. The graph's driver
        // writes the area from another thread: every word is read volatile, and the start time is
        // read twice so that a read across the start of a cycle is thrown away.
        unsafe {
            let clock = ptr::addr_of!((*self.position).clock);
            let nsec = ptr::addr_of!((*clock).nsec).read_volatile();
            let cycle = clock
                .cast::<u8>()
                .add(CLOCK_CYCLE_AT)
                .cast::<u32>()
                .read_volatile();
            let duration = ptr::addr_of!((*clock).duration).read_volatile();
            (ptr::addr_of!((*clock).nsec).read_volatile() == nsec).then_some((
                nsec,
                (cycle & 1) as u8,
                duration,
            ))
        }
    }

    /// How many are counted on buffer `index`, 0 when the buffers carry no busy count. The node
    /// itself is counted from when PipeWire gives the buffer back until it is sent again.
    fn busy_count(&self, index: usize) -> u32 {
        let busy = self.busy.get(index).copied().unwrap_or(ptr::null());
        if busy.is_null() {
            return 0;
        }
        // SAFETY: `busy` points into the metadata of a buffer PipeWire has added and not removed
        // (it is nulled in `remove_buffer`). The consumer changes the count from its own process,
        // so it is read as the atomic it is.
        unsafe {
            (*ptr::addr_of!((*busy).count).cast::<std::sync::atomic::AtomicU32>())
                .load(Ordering::Acquire)
        }
    }

    /// How many consumers are counted on buffer `index` now.
    fn consumers_on(&self, index: usize) -> u32 {
        self.busy_count(index)
            .saturating_sub(u32::from(self.holds.is_back(index)))
    }

    /// One look at the graph: counts the cycle, sees whether the buffer in flight has gone out
    /// (and to which io area), and frees every buffer no consumer has or can still take.
    fn settle(&mut self, seen: Option<Snapshot>) {
        if let Some(seen) = seen {
            if seen.now != self.last_now {
                self.last_now = seen.now;
                self.cycles += 1;
            }
            if let Some(flight) = self.in_flight
                && seen.pending == 0
            {
                // It went out in the cycle of this snapshot or an earlier one. It is this one
                // exactly when the snapshot before the send was the cycle before; and the area is
                // the current cycle's when this snapshot is of the current cycle.
                let area = match (flight.before, self.clock()) {
                    (Some(before), Some((nsec, bit, duration)))
                        if duration > 0
                            && seen.ticks.wrapping_sub(before.ticks) == duration
                            && seen.now as u64 == nsec =>
                    {
                        Area::Known(bit)
                    }
                    _ => Area::Unknown,
                };
                self.holds.went_out(flight.index, area);
                self.in_flight = None;
            }
        }
        let mut on = [0_u32; MAX_BUFFERS];
        for (index, on) in on.iter_mut().enumerate().take(self.token.len()) {
            *on = self.consumers_on(index);
        }
        let Self {
            holds,
            token,
            avail,
            n_avail,
            pump,
            cycles,
            ..
        } = self;
        holds.settle(
            *cycles,
            |index| on[index],
            |index| {
                if let Some(slot) = token[index].take() {
                    pump.release(slot, false);
                }
                avail[index] = true;
                *n_avail += 1;
            },
        );
    }

    /// Gives back every frame the node still holds. Used once the stream is gone.
    fn release_held(&mut self) {
        for index in 0..self.token.len() {
            if let Some(slot) = self.token[index].take() {
                self.pump.release(slot, false);
            }
        }
    }
}

/// PipeWire added `buffer`: it is ring buffer `n` (the count so far), and its memory is that
/// buffer's memfd. The buffer is the node's to fill until it is queued.
#[cfg(feature = "pipewire")]
fn on_add_buffer(ring: &RefCell<Ring>, buffer: *mut pw_sys::pw_buffer) {
    let mut r = ring.borrow_mut();
    let index = r.added;
    r.added += 1;
    if index >= r.pw.len() || buffer.is_null() {
        r.failed = true;
        return;
    }
    // SAFETY: PipeWire passes a live buffer of this stream to `add_buffer`. It stays valid until
    // `remove_buffer`, and this reads only its spa_buffer pointer and count.
    let (spa, n_datas) = unsafe {
        let spa = (*buffer).buffer;
        (spa, if spa.is_null() { 0 } else { (*spa).n_datas })
    };
    if n_datas < 1 {
        r.failed = true;
        return;
    }
    // SAFETY: `spa` is non-null with at least one data, so `datas` points at the first entry.
    let data = unsafe { (*spa).datas };
    // SAFETY: `data` is that first entry, valid while the buffer is.
    let data_type = unsafe { (*data).type_ };
    // The check the C shim made, kept as it was: the consumer's memfd bit is in the data type.
    if data_type & (1u32 << DataType::MemFd.as_raw()) == 0 {
        eprintln!("styx-pipewire: a consumer does not take memfd buffers");
        r.failed = true;
        return;
    }
    let fd = match r.fds[index].try_clone() {
        Ok(fd) => fd,
        Err(_) => {
            r.failed = true;
            return;
        }
    };
    // The buffer is `lens[index]` bytes at `offsets[index]` of its memfd. Every buffer is given
    // the same mapping: the memfd from its start, as long as the furthest buffer's end, with the
    // frame's chunk at the buffer's own offset. A consumer that maps its buffers itself needs a
    // page-aligned start, and PipeWire's stream refuses a set of buffers whose sizes differ
    // ("invalid buffer size"), so the mapping cannot start at, or be sized to, each buffer.
    let span = r
        .offsets
        .iter()
        .zip(&r.lens)
        .map(|(&offset, &len)| offset + u64::from(len))
        .max()
        .unwrap_or(0);
    let (Ok(maxsize), Ok(_chunk_offset)) = (u32::try_from(span), u32::try_from(r.offsets[index]))
    else {
        eprintln!("styx-pipewire: ring buffer {index} ends past what a PipeWire buffer can name");
        r.failed = true;
        return;
    };
    // SAFETY: `data` is the buffer's first data (checked above), and the buffer is the node's until
    // it is queued, so its descriptor fields are the node's to set. The memfd is a duplicate
    // handed to PipeWire, which owns it from here, as the C shim left it.
    unsafe {
        (*data).type_ = DataType::MemFd.as_raw();
        (*data).flags = DATA_FLAGS;
        (*data).fd = i64::from(fd.into_raw_fd());
        (*data).mapoffset = 0;
        (*data).maxsize = maxsize;
        (*data).data = ptr::null_mut();
        // SAFETY (same buffer): `user_data` records the ring index for the callbacks and the loop.
        (*buffer).user_data = index as *mut c_void;
    }
    // SAFETY: `spa` is this live buffer's spa_buffer (non-null, checked above). The lookup reads its
    // metadata table and returns a pointer into it, or null when the buffers carry no busy count.
    let busy = unsafe {
        pipewire::spa::sys::spa_buffer_find_meta_data(
            spa,
            pipewire::spa::sys::SPA_META_Busy,
            size_of::<pipewire::spa::sys::spa_meta_busy>(),
        )
    };
    r.busy[index] = busy
        .cast::<pipewire::spa::sys::spa_meta_busy>()
        .cast_const();
    // SAFETY: as for the busy count; null when the buffers carry no header.
    let header = unsafe {
        pipewire::spa::sys::spa_buffer_find_meta_data(
            spa,
            pipewire::spa::sys::SPA_META_Header,
            size_of::<pipewire::spa::sys::spa_meta_header>(),
        )
    };
    r.header[index] = header.cast::<pipewire::spa::sys::spa_meta_header>();
    r.pw[index] = buffer;
}

/// The buffers are the producer's, for a consumer to read: readable, and mappable (the flag a
/// PipeWire 1.2 or later stream wants before it maps a buffer for a consumer that asked; written
/// as its value, bit 3, since older headers do not name it and older libraries ignore it).
#[cfg(feature = "pipewire")]
const DATA_FLAGS: u32 = pipewire::spa::sys::SPA_DATA_FLAG_READABLE | (1 << 3);

/// PipeWire removed `buffer`. The frame in it, if any, is released; the buffer is no longer the
/// node's.
#[cfg(feature = "pipewire")]
fn on_remove_buffer(ring: &RefCell<Ring>, buffer: *mut pw_sys::pw_buffer) {
    if buffer.is_null() {
        return;
    }
    let mut r = ring.borrow_mut();
    // SAFETY: PipeWire passes a live buffer to `remove_buffer`; its `user_data` is the ring index
    // that `add_buffer` set (or the value PipeWire kept, for a buffer the node never used).
    let index = unsafe { (*buffer).user_data } as usize;
    if index >= r.pw.len() {
        return;
    }
    if let Some(slot) = r.token[index].take() {
        r.pump.release(slot, false);
    }
    if r.avail[index] {
        r.avail[index] = false;
        r.n_avail -= 1;
    }
    r.holds.forget(index);
    if r.in_flight.is_some_and(|flight| flight.index == index) {
        r.in_flight = None;
    }
    r.busy[index] = ptr::null();
    r.header[index] = ptr::null_mut();
    r.pw[index] = ptr::null_mut();
}

/// The chunk of a PipeWire buffer's first data, or null when it has none.
///
/// # Safety
///
/// `raw` must be a buffer of a stream that is live and has not removed it.
#[cfg(feature = "pipewire")]
unsafe fn chunk_of(raw: *mut pw_sys::pw_buffer) -> *mut pipewire::spa::sys::spa_chunk {
    // SAFETY: the caller guarantees `raw` is a live buffer, so its spa_buffer and datas are valid
    // while it is; a null or empty spa_buffer is checked before its datas are read.
    unsafe {
        let spa = (*raw).buffer;
        if spa.is_null() || (*spa).n_datas == 0 {
            return ptr::null_mut();
        }
        (*(*spa).datas).chunk
    }
}

/// Connects the stream with the current buffers. Errors: -2 for a format with no PipeWire name,
/// -1 when the pod cannot be built, -3 when the connect fails.
#[cfg(feature = "pipewire")]
fn connect_stream(stream: &Stream, format: Format) -> Result<(), i32> {
    let video = video_format(format.fourcc).ok_or(-2)?;
    let format_pod = serialize(&Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                FormatProperties::MediaType.0,
                Value::Id(Id(MediaType::Video.as_raw())),
            ),
            Property::new(
                FormatProperties::MediaSubtype.0,
                Value::Id(Id(MediaSubtype::Raw.as_raw())),
            ),
            Property::new(
                FormatProperties::VideoFormat.0,
                Value::Id(Id(video.as_raw())),
            ),
            Property::new(
                FormatProperties::VideoSize.0,
                Value::Rectangle(Rectangle {
                    width: format.width,
                    height: format.height,
                }),
            ),
            Property::new(
                FormatProperties::VideoFramerate.0,
                Value::Fraction(Fraction {
                    num: format.fps.0,
                    denom: format.fps.1,
                }),
            ),
        ],
    }))?;
    let buffers_pod = serialize(&Value::Object(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                pipewire::spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Int(format.buffers as i32),
            ),
            Property::new(pipewire::spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(
                pipewire::spa::sys::SPA_PARAM_BUFFERS_size,
                Value::Int(format.size as i32),
            ),
            Property::new(
                pipewire::spa::sys::SPA_PARAM_BUFFERS_stride,
                Value::Int(format.stride as i32),
            ),
            Property::new(
                pipewire::spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Int((1u32 << DataType::MemFd.as_raw()) as i32),
            ),
        ],
    }))?;
    // Each buffer carries a header for a consumer that asks for one: the frame's id as `seq` and
    // its capture time as `pts`. A consumer needs them: see the module's note on order.
    let header_pod = serialize(&Value::Object(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                pipewire::spa::sys::SPA_PARAM_META_type,
                Value::Id(pipewire::spa::utils::Id(
                    pipewire::spa::sys::SPA_META_Header,
                )),
            ),
            Property::new(
                pipewire::spa::sys::SPA_PARAM_META_size,
                Value::Int(size_of::<pipewire::spa::sys::spa_meta_header>() as i32),
            ),
        ],
    }))?;
    let format_pod = Pod::from_bytes(&format_pod).ok_or(-1)?;
    let buffers_pod = Pod::from_bytes(&buffers_pod).ok_or(-1)?;
    let header_pod = Pod::from_bytes(&header_pod).ok_or(-1)?;
    let mut params = [format_pod, buffers_pod, header_pod];
    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::ALLOC_BUFFERS,
            &mut params,
        )
        .map_err(|_| -3)
}

/// A pod's bytes for `value`. -1 when the pod cannot be serialized.
#[cfg(feature = "pipewire")]
fn serialize(value: &Value) -> Result<Vec<u8>, i32> {
    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), value).map_err(|_| -1)?;
    Ok(cursor.into_inner())
}

/// Connects the stream again with the new producer's buffers. Frames the node held were dropped
/// with the old producer, so no token is released. Any failure ends the run (-4).
#[cfg(feature = "pipewire")]
fn reconfigure(ring: &RefCell<Ring>, stream: &Stream, buffers: Buffers) -> Result<(), i32> {
    ring.borrow_mut().forget_held();
    // The stream's buffers go with it, and their removal reaches the callbacks here.
    let _ = stream.disconnect();
    let format = {
        let mut r = ring.borrow_mut();
        r.adopt(buffers)?;
        r.format
    };
    connect_stream(stream, format)
}

/// One tick of the loop: stop if asked, reconnect if the producer's buffers are new, take back the
/// buffers PipeWire has returned, and queue frames into the free ones. Never blocks.
#[cfg(feature = "pipewire")]
fn tick(ring: &RefCell<Ring>, stream: &Stream, quit: &MainLoopRc, control: &Control) {
    if control.stopped() {
        quit.quit();
        return;
    }
    let taken = ring.borrow_mut().pump.take(Instant::now());
    if let Some(buffers) = taken {
        ring.borrow_mut().pump.stats.streams += 1;
        if reconfigure(ring, stream, buffers).is_err() {
            ring.borrow_mut().failed = true;
            quit.quit();
            ring.borrow().pump.publish();
            return;
        }
    }
    // What PipeWire gives back has left the link's queue, which is not the consumer's word that it
    // is done: see `hold`. A buffer a consumer is counted on is refused back here and tried again
    // on a later tick, so the stream is asked once per buffer.
    let asks = ring.borrow().pw.len();
    for _ in 0..asks {
        if !ring.borrow().io {
            break;
        }
        // SAFETY: this runs on the loop's thread inside its timer, so no other call on `stream` is
        // in progress. The buffer returned is the node's until it is queued below.
        let raw = unsafe { stream.dequeue_raw_buffer() };
        if raw.is_null() {
            continue;
        }
        // SAFETY: `raw` is a non-null buffer PipeWire has just returned, so its user_data is the
        // ring index that `add_buffer` set.
        let index = unsafe { (*raw).user_data } as usize;
        let mut r = ring.borrow_mut();
        if index >= r.avail.len() {
            continue;
        }
        if r.holds.is_held(index) {
            r.holds.came_back(index);
        } else if !r.avail[index] {
            // A new buffer's first return: it has never held a frame.
            r.avail[index] = true;
            r.n_avail += 1;
        }
    }
    let seen = snapshot(stream);
    ring.borrow_mut().settle(seen);
    // Pull a frame only when a consumer is linked, a buffer is free for it, and the last send
    // has gone out: one buffer is in flight at a time.
    loop {
        let pulled = {
            let mut r = ring.borrow_mut();
            if !r.io || r.n_avail == 0 || r.in_flight.is_some() {
                break;
            }
            match r.pump.next() {
                Some(pulled) => pulled,
                None => break,
            }
        };
        let index = pulled.index;
        let raw = {
            let r = ring.borrow();
            match (r.avail.get(index), r.pw.get(index)) {
                (Some(true), Some(&raw)) if !raw.is_null() => raw,
                _ => ptr::null_mut(),
            }
        };
        let chunk = if raw.is_null() {
            ptr::null_mut()
        } else {
            // SAFETY: `raw` is a buffer the node holds (it is `avail`, and PipeWire has not removed
            // it), so it is live.
            unsafe { chunk_of(raw) }
        };
        if chunk.is_null() {
            ring.borrow_mut().pump.release(pulled.slot, true);
            continue;
        }
        // Checked to fit when the buffer was added.
        let lead = ring.borrow().offsets.get(index).map_or(0, |&o| o as u32);
        // SAFETY: `chunk` is the chunk of a buffer the node holds (checked above), so it is the
        // node's to write until the buffer is queued.
        unsafe {
            (*chunk).offset = lead;
            (*chunk).size = pulled.len;
            (*chunk).stride = pulled.stride as i32;
            (*chunk).flags = 0;
        }
        let header = ring
            .borrow()
            .header
            .get(index)
            .copied()
            .unwrap_or(ptr::null_mut());
        if !header.is_null() {
            // SAFETY: `header` points into the metadata of this buffer, which the node holds
            // (checked above) and so may write until it is queued.
            unsafe {
                (*header).flags = 0;
                (*header).offset = 0;
                (*header).pts = pulled.timestamp_ns as i64;
                (*header).dts_offset = 0;
                (*header).seq = pulled.id;
            }
        }
        {
            let mut r = ring.borrow_mut();
            r.token[index] = Some(pulled.slot);
            r.avail[index] = false;
            r.n_avail -= 1;
            r.holds.send(index);
            r.in_flight = Some(InFlight {
                index,
                before: snapshot(stream),
            });
            r.last_send = r.cycles;
        }
        // SAFETY: `raw` was dequeued from this stream and is filled now. It is queued once, here,
        // which is the return path PipeWire expects for it. Its size is one, so that PipeWire's
        // count of what is queued says when it has gone out.
        unsafe {
            (*raw).size = 1;
            stream.queue_raw_buffer(raw);
        }
    }
    // The frames the node holds leave the producer nothing to write to: all its buffers but one
    // (which it keeps for its newest frame) hold a frame a consumer could still take, and no
    // frame has come for a while. Waiting changes nothing, so the oldest held frame goes out
    // again: it replaces what is on the link, and a consumer that is running takes it.
    let again = {
        let r = ring.borrow();
        let starved = r.holds.held() + 1 >= r.pw.len() && r.cycles >= r.last_send + RESEND_CYCLES;
        if r.io && r.in_flight.is_none() && starved {
            r.holds
                .oldest_held(|index| r.consumers_on(index) > 0)
                .map(|index| (index, r.pw[index]))
        } else {
            None
        }
    };
    if let Some((index, raw)) = again
        && !raw.is_null()
    {
        {
            let mut r = ring.borrow_mut();
            r.holds.send(index);
            r.in_flight = Some(InFlight {
                index,
                before: seen,
            });
            r.last_send = r.cycles;
            r.pump.stats.resent += 1;
        }
        // SAFETY: `raw` is a buffer the node has back from PipeWire (`oldest_held` gives only
        // those) with its frame, chunk and header as they were sent. It is queued once, here.
        unsafe {
            (*raw).size = 1;
            stream.queue_raw_buffer(raw);
        }
    }
    ring.borrow().pump.publish();
}

/// Runs the PipeWire loop for the first producer's buffers, and reconnects on new ones, until
/// stopped. Returns 0 when it ran, or the start or buffer failure code. The stream and loop are
/// destroyed before it returns.
#[cfg(feature = "pipewire")]
fn stream_loop(ring: &Rc<RefCell<Ring>>, control: &Control, name: &str, first: Buffers) -> i32 {
    if video_format(first.fourcc.to_u32()).is_none() {
        return -2;
    }
    if ring.borrow_mut().adopt(first).is_err() {
        return -1;
    }
    let format = ring.borrow().format;
    pipewire::init();
    let Ok(mainloop) = MainLoopRc::new(None) else {
        return -1;
    };
    let Ok(context) = ContextRc::new(&mainloop, None) else {
        return -1;
    };
    let Ok(core) = context.connect_rc(None) else {
        return -1;
    };
    let props = properties! {
        *keys::MEDIA_TYPE => "Video",
        *keys::MEDIA_CATEGORY => "Source",
        *keys::MEDIA_CLASS => "Video/Source",
        *keys::MEDIA_ROLE => "Camera",
        *keys::NODE_NAME => name,
        *keys::NODE_DESCRIPTION => name,
    };
    let Ok(stream) = StreamRc::new(core, name, props) else {
        return -1;
    };
    let Ok(_listener) = stream
        .add_local_listener_with_user_data(Rc::clone(ring))
        .add_buffer(|_, shared, buffer| on_add_buffer(shared, buffer))
        .remove_buffer(|_, shared, buffer| on_remove_buffer(shared, buffer))
        .io_changed(|_, shared, id, area, size| {
            if id == pipewire::spa::sys::SPA_IO_Buffers {
                shared.borrow_mut().io = !area.is_null();
            } else if id == pipewire::spa::sys::SPA_IO_Position {
                let whole = size as usize >= size_of::<pipewire::spa::sys::spa_io_position>();
                shared.borrow_mut().position = if whole {
                    area.cast::<pipewire::spa::sys::spa_io_position>()
                        .cast_const()
                } else {
                    ptr::null()
                };
            }
        })
        .register()
    else {
        return -1;
    };
    if let Err(code) = connect_stream(&stream, format) {
        return code;
    }
    let timer = {
        let shared = Rc::clone(ring);
        let control = control.share();
        let quit = mainloop.clone();
        let stream = stream.clone();
        mainloop
            .loop_()
            .add_timer(move |_expirations| tick(&shared, &stream, &quit, &control))
    };
    let _ = timer.update_timer(Some(TICK), Some(TICK));
    mainloop.run();
    let failed = ring.borrow().failed;
    if failed { -4 } else { 0 }
}

#[cfg(feature = "pipewire")]
fn run_built(
    config: &NodeConfig,
    control: &Control,
    min_buffers: usize,
) -> Result<NodeStats, NodeError> {
    if config.name.contains('\0') {
        return Err(NodeError::BadConfig("the node name has a NUL byte"));
    }
    let fps = (config.fps.0, config.fps.1.max(1));
    let mut pump = Pump {
        control: control.share(),
        link: Link::new(&config.socket),
        slots: Vec::new(),
        stats: NodeStats::default(),
        min_buffers,
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
    pump.stats.streams += 1;
    let ring = Rc::new(RefCell::new(Ring::new(pump, fps)));
    let rc = stream_loop(&ring, control, &config.name, first);
    // The stream and loop are gone: the frames still held are PipeWire's no longer, so release them.
    let mut r = ring.borrow_mut();
    r.release_held();
    r.pump.link.stop();
    r.pump.publish();
    let stats = r.pump.stats;
    if rc != 0 {
        return Err(NodeError::Start(rc));
    }
    Ok(stats)
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

    #[cfg(feature = "pipewire")]
    #[test]
    fn the_formats_map_to_the_video_names_the_shim_used() {
        assert_eq!(video_format(0x3231_564e), Some(VideoFormat::NV12));
        assert_eq!(video_format(0x3231_5559), Some(VideoFormat::I420));
        assert_eq!(video_format(0x5956_5955), Some(VideoFormat::UYVY));
        assert_eq!(video_format(0x1234_5678), None);
    }
}
