//! The node: a PipeWire `Video/Source` stream whose buffers are a producer's memfds. The node
//! outlives its producer. When the producer goes away the stream stays up and idle, the link backs
//! off (250 ms doubling to 5 s), and when a producer returns the node learns its buffers and
//! connects the stream again with them. The stream is pipewire-rs; its loop ticks every 2 ms.
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
        run_built(config, control)
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

/// How many graph cycles after a buffer went out the node waits before it reads the buffer's busy
/// count. PipeWire copies a sent buffer's id to the link's io area, where the consumer can take it
/// in the cycle it was sent or (a consumer that runs one cycle behind) the next; the cycle after
/// that the area is written again. From the third cycle on, a consumer that took the buffer has
/// counted itself on it, and one that did not can no longer take it.
#[cfg(feature = "pipewire")]
const TAKE_CYCLES: u64 = 3;

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
}

/// A frame taken from the producer, held in `slot` until it is queued or dropped.
#[cfg(feature = "pipewire")]
struct Pulled {
    index: usize,
    len: u32,
    stride: u32,
    slot: usize,
}

#[cfg(feature = "pipewire")]
impl Pump {
    /// Drives the link at `now`. Buffers come back only when the producer's format has a PipeWire
    /// name; any other format is refused and the link waits, as for a lost producer.
    fn take(&mut self, now: Instant) -> Option<Buffers> {
        let buffers = self.link.drive(now)?;
        if video_format(buffers.fourcc.to_u32()).is_none() {
            let why = format!("format {:?} has no PipeWire video name", buffers.fourcc);
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
        self.stats.frames += 1;
        self.slots[slot] = Some(frame);
        Some(Pulled {
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
    /// The clock position last read from it, and how many times it was seen to change: a count of
    /// graph cycles that can only run behind the true one.
    last_position: u64,
    cycles: u64,
    /// The cycle the last queued buffer goes out in (one buffer goes out per cycle).
    out_cycle: u64,
    /// For each buffer PipeWire gave back: the cycle from which its busy count says whether a
    /// consumer holds it. `None` while the buffer is queued, or free.
    back: Vec<Option<u64>>,
    /// The cycle from which each queued buffer's busy count can be read.
    settled: Vec<u64>,
    /// Each buffer's busy count, null when the buffers carry none.
    busy: Vec<*const pipewire::spa::sys::spa_meta_busy>,
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
            last_position: 0,
            cycles: 0,
            out_cycle: 0,
            back: Vec::new(),
            settled: Vec::new(),
            busy: Vec::new(),
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
        if count == 0 || lens.len() != count || offsets.len() != count {
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
        self.back = vec![None; count];
        self.settled = vec![0; count];
        self.busy = vec![ptr::null(); count];
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
        self.back.fill(None);
        self.n_avail = 0;
    }

    /// Counts a graph cycle when the clock has moved since the last look.
    fn watch_clock(&mut self) {
        if self.position.is_null() {
            return;
        }
        // SAFETY: `position` is the area PipeWire gave the stream and has not taken back (it is
        // nulled in `io_changed` when it does). The graph's driver writes it from another thread,
        // so this is a volatile read of one word, used only to see that it changed.
        let now = unsafe { ptr::addr_of!((*self.position).clock.position).read_volatile() };
        if now != self.last_position {
            self.last_position = now;
            self.cycles += 1;
        }
    }

    /// Whether a consumer has counted itself on buffer `index`. The node's own dequeue counts one.
    fn consumer_holds(&self, index: usize) -> bool {
        let busy = self.busy.get(index).copied().unwrap_or(ptr::null());
        if busy.is_null() {
            return false;
        }
        // SAFETY: `busy` points into the metadata of a buffer PipeWire has added and not removed
        // (it is nulled in `remove_buffer`). The consumer changes the count from its own process,
        // so it is read as the atomic it is.
        let count = unsafe {
            (*ptr::addr_of!((*busy).count).cast::<std::sync::atomic::AtomicU32>())
                .load(Ordering::Acquire)
        };
        count > 1
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
    // The buffer is `lens[index]` bytes at `offsets[index]` of its memfd. A consumer maps from
    // `mapoffset`, which must sit on a page boundary, so the mapping starts at the boundary below
    // the buffer and the frame's chunk starts `lead` bytes in (see `lead_of`).
    let offset = r.offsets[index];
    let lead = lead_of(offset);
    let (Ok(mapoffset), Some(maxsize)) = (
        u32::try_from(offset - u64::from(lead)),
        r.lens[index].checked_add(lead),
    ) else {
        eprintln!("styx-pipewire: ring buffer {index} starts past what a PipeWire buffer can name");
        r.failed = true;
        return;
    };
    // SAFETY: `data` is the buffer's first data (checked above), and the buffer is the node's until
    // it is queued, so its descriptor fields are the node's to set. The memfd is a duplicate
    // handed to PipeWire, which owns it from here, as the C shim left it.
    unsafe {
        (*data).type_ = DataType::MemFd.as_raw();
        (*data).flags = spa_sys_flag_readable();
        (*data).fd = i64::from(fd.into_raw_fd());
        (*data).mapoffset = mapoffset;
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
    r.pw[index] = buffer;
}

#[cfg(feature = "pipewire")]
fn spa_sys_flag_readable() -> u32 {
    pipewire::spa::sys::SPA_DATA_FLAG_READABLE
}

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
    r.back[index] = None;
    r.busy[index] = ptr::null();
    r.pw[index] = ptr::null_mut();
}

/// The boundary a consumer's mapping of a memfd starts on: a multiple of every page size in use
/// (4, 16 and 64 KiB), as `styx-frame` maps its own buffers.
#[cfg(feature = "pipewire")]
const MAP_ALIGN: u64 = 64 * 1024;

/// How far into its mapping a buffer at `offset` of its memfd starts.
#[cfg(feature = "pipewire")]
fn lead_of(offset: u64) -> u32 {
    (offset % MAP_ALIGN) as u32
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
    let format_pod = Pod::from_bytes(&format_pod).ok_or(-1)?;
    let buffers_pod = Pod::from_bytes(&buffers_pod).ok_or(-1)?;
    let mut params = [format_pod, buffers_pod];
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
    ring.borrow_mut().watch_clock();
    // A buffer PipeWire gives back has left the link, which is not yet the consumer's word that it
    // is done: PipeWire returns a buffer one cycle after sending it, whether or not the consumer
    // has taken it. So the buffer waits here until its busy count can be read (`TAKE_CYCLES`).
    while ring.borrow().io {
        // SAFETY: this runs on the loop's thread inside its timer, so no other call on `stream` is
        // in progress. The buffer returned is the node's until it is queued below.
        let raw = unsafe { stream.dequeue_raw_buffer() };
        if raw.is_null() {
            break;
        }
        // SAFETY: `raw` is a non-null buffer PipeWire has just returned, so its user_data is the
        // ring index that `add_buffer` set.
        let index = unsafe { (*raw).user_data } as usize;
        let mut r = ring.borrow_mut();
        if index < r.back.len() && !r.avail[index] {
            r.back[index] = Some(r.settled[index]);
        }
    }
    // A buffer is free once that cycle has come and no consumer is counted on it: the consumer is
    // done with the frame in it, or never took it.
    {
        let mut r = ring.borrow_mut();
        for index in 0..r.back.len() {
            let Some(settled) = r.back[index] else {
                continue;
            };
            if r.cycles < settled || r.consumer_holds(index) {
                continue;
            }
            r.back[index] = None;
            if let Some(slot) = r.token[index].take() {
                r.pump.release(slot, false);
            }
            r.avail[index] = true;
            r.n_avail += 1;
        }
    }
    // Only pull a frame when a buffer is free for it.
    loop {
        let pulled = {
            let mut r = ring.borrow_mut();
            if r.n_avail == 0 {
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
        let lead = ring.borrow().offsets.get(index).map_or(0, |&o| lead_of(o));
        // SAFETY: `chunk` is the chunk of a buffer the node holds (checked above), so it is the
        // node's to write until the buffer is queued.
        unsafe {
            (*chunk).offset = lead;
            (*chunk).size = pulled.len;
            (*chunk).stride = pulled.stride as i32;
            (*chunk).flags = 0;
        }
        {
            let mut r = ring.borrow_mut();
            r.token[index] = Some(pulled.slot);
            r.avail[index] = false;
            r.n_avail -= 1;
            // One buffer goes out per cycle, the first in the cycle after this one.
            r.out_cycle = r.out_cycle.max(r.cycles) + 1;
            r.settled[index] = r.out_cycle + TAKE_CYCLES;
        }
        // SAFETY: `raw` was dequeued from this stream and is filled now. It is queued once, here,
        // which is the return path PipeWire expects for it.
        unsafe { stream.queue_raw_buffer(raw) };
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
fn run_built(config: &NodeConfig, control: &Control) -> Result<NodeStats, NodeError> {
    if config.name.contains('\0') {
        return Err(NodeError::BadConfig("the node name has a NUL byte"));
    }
    let fps = (config.fps.0, config.fps.1.max(1));
    let mut pump = Pump {
        control: control.share(),
        link: Link::new(&config.socket),
        slots: Vec::new(),
        stats: NodeStats::default(),
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
