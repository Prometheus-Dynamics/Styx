//! Streaming: negotiate a format, queue the driver's buffers, and turn each dequeued buffer into a
//! frame.
//!
//! Buffers are requested as MMAP. When every one exports as a dma-buf (`VIDIOC_EXPBUF`), each is
//! registered once in a [`FramePool`] over an [`FdMemory`], and a dequeued buffer is handed on as
//! the frame that holds its index: the buffer goes back to the driver when no lease holds it. When
//! a buffer does not export, it is mapped and each frame is copied out of it, and the buffer goes
//! straight back to the driver; the copy is counted.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use styx_controls::{Control, ControlValue, FrameMetadata};
use styx_format::{FourCc, Interval, MediaFormat, Resolution};
use styx_frame::{
    BufferSize, FdMemory, FrameEntry, FrameLease, FramePool, HostMemory, Layouts, PlaneLayout,
};
use styx_lemnos::v4l2::v4l2::{
    BufType, ControlType, ControlValue as Raw, Controls, DequeuedBuffer, DmaBufAccess, Format,
    Memory, QueueBuffer, VideoDevice,
};
use styx_lemnos::v4l2::{Fraction, Mapping, monotonic_now};

use crate::controls::Table;
use crate::{Error, RawControl, layout, probe, record_mmap_copy};

/// How long the worker waits for a frame before it looks at its commands again.
const POLL: Duration = Duration::from_millis(50);
/// `V4L2_BUF_FLAG_TIMESTAMP_MASK`, and its `MONOTONIC` value.
const TIMESTAMP_MASK: u32 = 0xE000;
const TIMESTAMP_MONOTONIC: u32 = 0x2000;
/// `V4L2_BUF_FLAG_ERROR`: the driver flags a frame it could not fill.
const BUF_ERROR: u32 = 0x0000_0040;
/// The one flag of a single-planar format that a capture may keep.
const PREMULTIPLIED_ALPHA: u32 = 1;
/// Retry backoff: 250 ms doubling per failed attempt, never more than five seconds.
const BACKOFF_FIRST: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// What to stream: the format and size asked for, and how to run it.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamConfig {
    /// The pixel format, as Styx names it.
    pub code: FourCc,
    pub width: u32,
    pub height: u32,
    /// Seconds per frame to ask the driver for. `None` keeps the driver's.
    pub interval: Option<Interval>,
    /// V4L2 buffers to request (clamped to 2 through 16).
    pub buffers: u32,
    /// Controls set when the stream starts. A control the node lacks fails the start.
    pub controls: Vec<(Control, ControlValue)>,
    /// The failure policy. `None` (the default) retries forever. `Some(n)` stops the stream after
    /// `n` failed reconnects in a row, a policy the caller chose.
    pub give_up_after: Option<u32>,
    /// Copy every frame out of the mapped driver buffers, never exporting them as dma-bufs (no
    /// `VIDIOC_EXPBUF`). Off by default: a node whose buffers export is then zero-copy. Used to
    /// exercise the copy path on a node that exports.
    pub copy_mapped: bool,
}

impl StreamConfig {
    /// A stream of `code` at `width` x `height`, with the driver's interval, four buffers, no
    /// controls, and retries that never give up.
    pub fn new(code: FourCc, width: u32, height: u32) -> Self {
        Self {
            code,
            width,
            height,
            interval: None,
            buffers: 4,
            controls: Vec::new(),
            give_up_after: None,
            copy_mapped: false,
        }
    }
}

/// How frames reach the stream's consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Every buffer is a dma-buf: frames are the driver's memory, not copies.
    Dmabuf,
    /// The buffers are MMAP: each frame is a copy, counted in `copy_counters`.
    Copied,
}

/// Where a stream is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Streaming,
    /// The device is lost or failing: the next attempt is in `backoff`, after `attempt` failures.
    Retrying {
        attempt: u32,
        backoff: Duration,
        error: String,
    },
    /// Stopped by the caller.
    Stopped,
    /// The configured policy gave up after too many failed reconnects.
    Failed {
        error: String,
    },
}

/// A snapshot of a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub delivery: Delivery,
    /// Why this delivery was chosen.
    pub delivery_note: String,
    /// Frames delivered to the consumer.
    pub frames: u64,
    /// Frames dropped: no free buffer, or the consumer was not keeping up.
    pub dropped: u64,
    /// Frames whose timestamp is the arrival time, because the driver did not flag its own as
    /// monotonic. The rest carry the driver's timestamp.
    pub arrival_stamped: u64,
    /// Successful reconnects since the stream started.
    pub reconnects: u32,
    /// Control values the driver refused when they were reapplied after a reconnect.
    pub control_errors: u32,
}

/// A running stream: frames arrive on [`Stream::frames`] until the stream stops.
///
/// Dropping the stream stops it and waits for its thread.
pub struct Stream {
    format: MediaFormat,
    frames: Receiver<FrameLease<FrameMetadata>>,
    commands: mpsc::Sender<Command>,
    shared: Arc<Shared>,
    devices: Vec<RawControl>,
    worker: Option<JoinHandle<()>>,
}

impl Stream {
    /// Opens `path`, negotiates `config`, queues the buffers and starts streaming. Errors here
    /// are the caller's: the node is missing, refuses the format, or has no mapping for a control.
    pub fn start(path: impl AsRef<Path>, config: StreamConfig) -> Result<Stream, Error> {
        let path = path.as_ref().to_path_buf();
        let session = Session::open(&path, &config)?;
        let table = session.table;
        let mut wanted = [ControlValue::Auto; Control::COUNT];
        for control in Control::ALL {
            wanted[control.index()] = table
                .read(&session.dev, session.buf_type, control)
                .unwrap_or_else(|| control.spec().default);
        }
        for &(control, value) in &config.controls {
            table.apply(&session.dev, session.buf_type, control, value)?;
            wanted[control.index()] = value;
        }
        let format = session.media;
        let devices = session.devices.clone();
        let (frames_tx, frames_rx) = mpsc::sync_channel(session.queued.len().max(1));
        let (commands_tx, commands_rx) = mpsc::channel();
        let shared = Arc::new(Shared::new(session.delivery, session.note.clone()));
        let env = Env {
            path,
            config,
            shared: Arc::clone(&shared),
            commands: commands_rx,
            frames: frames_tx,
            wanted,
            table,
            devices: devices.clone(),
            attempt: 0,
            last_error: String::new(),
        };
        let worker = thread::Builder::new()
            .name("styx-v4l2".into())
            .spawn(move || run(env, session))?;
        Ok(Stream {
            format,
            frames: frames_rx,
            commands: commands_tx,
            shared,
            devices,
            worker: Some(worker),
        })
    }

    /// The format the driver gave: what every frame of this stream is.
    pub fn format(&self) -> MediaFormat {
        self.format
    }

    /// The frames, in capture order. Drop a lease to give its buffer back.
    pub fn frames(&self) -> &Receiver<FrameLease<FrameMetadata>> {
        &self.frames
    }

    /// Where the stream is now.
    pub fn status(&self) -> Status {
        let live = self
            .shared
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Status {
            state: live.state,
            delivery: live.delivery,
            delivery_note: live.note,
            frames: self.shared.frames.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            arrival_stamped: self.shared.arrival_stamped.load(Ordering::Relaxed),
            reconnects: self.shared.reconnects.load(Ordering::Relaxed),
            control_errors: self.shared.control_errors.load(Ordering::Relaxed),
        }
    }

    /// Sets `control` to `value` now. Applies on the next frame the driver takes. While the
    /// device is lost, the value is kept and applied when it comes back.
    pub fn set_control(&self, control: Control, value: ControlValue) -> Result<(), Error> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Set {
                control,
                value,
                reply,
            })
            .map_err(|_| Error::Stopped)?;
        answer.recv().map_err(|_| Error::Stopped)?
    }

    /// Every control the node offers, as probed when the stream started: mapped to a Styx control
    /// or not. Nothing is left out; see [`RawControl::mapped`].
    pub fn device_controls(&self) -> &[RawControl] {
        &self.devices
    }

    /// The controls with no Styx control: read and set by id with [`Stream::get_device_control`]
    /// and [`Stream::set_device_control`]. Their values are not in the frame metadata.
    pub fn passthrough(&self) -> impl Iterator<Item = &RawControl> {
        self.devices
            .iter()
            .filter(|control| control.mapped.is_none())
    }

    /// The value of a control by its id, read from the device now. A passthrough control, or a
    /// mapped one (its Styx control reports it in the frame metadata). Refused while the device is
    /// lost, since there is no device to ask.
    pub fn get_device_control(&self, id: u32) -> Result<i64, Error> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(Command::GetDevice { id, reply })
            .map_err(|_| Error::Stopped)?;
        answer.recv().map_err(|_| Error::Stopped)?
    }

    /// Sets a passthrough control by its id on the device now. Mapped controls are set with
    /// [`Stream::set_control`] instead, so that the frame metadata stays true. Passthrough values
    /// are not replayed after a reconnect: the caller sets them again if the device forgot them.
    pub fn set_device_control(&self, id: u32, value: i64) -> Result<(), Error> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(Command::SetDevice { id, value, reply })
            .map_err(|_| Error::Stopped)?;
        answer.recv().map_err(|_| Error::Stopped)?
    }

    /// Stops the stream and waits for its thread. Same as dropping it.
    pub fn stop(self) {}

    /// Stops the stream, waits for its thread, and returns its final status. After this the
    /// counts are settled: no frame is copied or delivered later, so counters read afterwards
    /// (the copy counters, for one) match the status exactly.
    pub fn finish(mut self) -> Status {
        let _ = self.commands.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.status()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream")
            .field("format", &self.format)
            .field("status", &self.status())
            .finish()
    }
}

enum Command {
    Set {
        control: Control,
        value: ControlValue,
        reply: SyncSender<Result<(), Error>>,
    },
    GetDevice {
        id: u32,
        reply: SyncSender<Result<i64, Error>>,
    },
    SetDevice {
        id: u32,
        value: i64,
        reply: SyncSender<Result<(), Error>>,
    },
    Stop,
}

/// What the stream's consumers and the worker share.
#[derive(Debug)]
struct Shared {
    frames: AtomicU64,
    dropped: AtomicU64,
    arrival_stamped: AtomicU64,
    reconnects: AtomicU32,
    control_errors: AtomicU32,
    live: Mutex<Live>,
}

#[derive(Debug, Clone)]
struct Live {
    state: State,
    delivery: Delivery,
    note: String,
}

impl Shared {
    fn new(delivery: Delivery, note: String) -> Self {
        Self {
            frames: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            arrival_stamped: AtomicU64::new(0),
            reconnects: AtomicU32::new(0),
            control_errors: AtomicU32::new(0),
            live: Mutex::new(Live {
                state: State::Streaming,
                delivery,
                note,
            }),
        }
    }

    fn set(&self, state: State, delivery: Delivery, note: String) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        *live = Live {
            state,
            delivery,
            note,
        };
    }

    fn set_state(&self, state: State) {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state = state;
    }
}

/// One open device: the queue, its buffers and the frame pool they feed.
struct Session {
    dev: VideoDevice,
    buf_type: BufType,
    table: Table,
    /// Every control the node offers, probed when the session opened.
    devices: Vec<RawControl>,
    media: MediaFormat,
    /// The layout of an uncompressed frame; `None` for compressed frames.
    layouts: Option<Layouts>,
    /// Bytes an uncompressed frame spans.
    span: usize,
    /// Bytes a frame may use: the pool's buffer size.
    payload: usize,
    compressed: bool,
    pool: FramePool<FrameMetadata>,
    /// Mapped buffers, when frames are copied out of them. Empty for DMABUF delivery.
    maps: Vec<Mapping>,
    /// Whether each buffer is queued with the driver.
    queued: Vec<bool>,
    delivery: Delivery,
    note: String,
}

struct Shape {
    media: MediaFormat,
    layouts: Option<Layouts>,
    span: usize,
    payload: usize,
    compressed: bool,
}

/// The format the driver settled on.
#[derive(Debug, Clone, Copy)]
struct Negotiated {
    code: [u8; 4],
    width: u32,
    height: u32,
    bytes_per_line: u32,
    size_image: u32,
}

impl Shape {
    fn new(got: &Negotiated) -> Result<Shape, Error> {
        let code = FourCc::new(got.code);
        let resolution = Resolution::new(got.width, got.height)
            .ok_or_else(|| Error::Unsupported("the driver gave a zero-sized frame".into()))?;
        let media = MediaFormat::with_defaults(code, resolution);
        let size_image = got.size_image as usize;
        if code.is_compressed() {
            if size_image == 0 {
                return Err(Error::Unsupported(format!(
                    "{code}: the driver gave no maximum frame size"
                )));
            }
            return Ok(Shape {
                media,
                layouts: None,
                span: 0,
                payload: size_image,
                compressed: true,
            });
        }
        let layouts = layout::uncompressed(code, got.width, got.height, got.bytes_per_line)?;
        let span = layouts
            .span()
            .ok_or_else(|| Error::Unsupported(format!("{code}: no frame span")))?;
        Ok(Shape {
            media,
            layouts: Some(layouts),
            span,
            payload: span.max(size_image),
            compressed: false,
        })
    }
}

impl Session {
    fn open(path: &Path, config: &StreamConfig) -> Result<Session, Error> {
        let dev = VideoDevice::open(path)?;
        let (_queue, buf_type) = probe::queue_of(&dev)
            .ok_or_else(|| Error::Unsupported(format!("{}: not a capture node", path.display())))?;
        let got = negotiate(&dev, buf_type, config)?;
        let shape = Shape::new(&got)?;
        if let Some(interval) = config.interval {
            dev.set_frame_interval(
                buf_type,
                Fraction::new(interval.numerator.get(), interval.denominator.get()),
            )?;
        }
        let infos = dev.query_controls().unwrap_or_default();
        let table = Table::from_infos(&dev, &infos, buf_type);
        let devices = infos
            .iter()
            .map(|info| RawControl::from_info(&dev, info))
            .collect::<Vec<_>>();
        let count = dev
            .request_buffers(buf_type, Memory::Mmap, config.buffers.clamp(2, 16))?
            .count as usize;
        if count == 0 {
            return Err(Error::Unsupported("the driver granted no buffers".into()));
        }
        let mut lengths = Vec::with_capacity(count);
        for index in 0..count {
            let info = dev.query_buffer(buf_type, index as u32)?;
            let length = info.planes.first().map_or(0, |p| p.length as usize);
            if length < shape.payload {
                return Err(Error::Unsupported(format!(
                    "buffer {index} holds {length} bytes, a frame needs {}",
                    shape.payload
                )));
            }
            lengths.push(length);
        }
        let exported = if config.copy_mapped {
            Err("copying is forced by the stream's configuration".to_string())
        } else {
            export_pool(&dev, buf_type, &lengths, shape.payload)
        };
        let (delivery, note, pool, maps) = match exported {
            Ok(pool) => (
                Delivery::Dmabuf,
                "every buffer exports as a dma-buf; frames are not copied".to_string(),
                pool,
                Vec::new(),
            ),
            Err(why) => {
                let (pool, maps) = copied_pool(&dev, buf_type, count, shape.payload)?;
                (
                    Delivery::Copied,
                    format!("buffers do not export as dma-bufs ({why}); frames are copied"),
                    pool,
                    maps,
                )
            }
        };
        let mut session = Session {
            dev,
            buf_type,
            table,
            devices,
            media: shape.media,
            layouts: shape.layouts,
            span: shape.span,
            payload: shape.payload,
            compressed: shape.compressed,
            pool,
            maps,
            queued: vec![false; count],
            delivery,
            note,
        };
        for index in 0..count {
            session.queue(index)?;
        }
        session.dev.stream_on(buf_type)?;
        Ok(session)
    }

    fn queue(&mut self, index: usize) -> Result<(), Error> {
        self.dev
            .queue(&QueueBuffer::mmap(self.buf_type, index as u32))?;
        self.queued[index] = true;
        Ok(())
    }

    /// Queues every dma-buf buffer that no frame holds and the driver does not have.
    fn requeue_free(&mut self) -> Result<(), Error> {
        if self.delivery != Delivery::Dmabuf {
            return Ok(());
        }
        for index in 0..self.queued.len() {
            if !self.queued[index] && self.pool.is_free(index) {
                self.queue(index)?;
            }
        }
        Ok(())
    }

    /// The bytes of a frame from a buffer with `used` bytes, or `None` for a frame to drop.
    fn frame_len(&self, used: usize, errored: bool) -> Option<usize> {
        if errored {
            return None;
        }
        if self.compressed {
            (used > 0 && used <= self.payload).then_some(used)
        } else {
            Some(self.span)
        }
    }

    fn entry(&self, sequence: u32, stamp: u64, len: usize) -> FrameEntry {
        let layouts = match self.layouts {
            Some(layouts) => layouts,
            // A compressed frame is one plane of the bytes it took; `len <= payload` was checked.
            None => Layouts::new(&[PlaneLayout::new(0, len, len)]).unwrap_or(Layouts::EMPTY),
        };
        FrameEntry::new(u64::from(sequence), stamp, self.media, layouts)
    }

    /// Stops the stream. The device closes when the session drops.
    fn stop(&mut self) {
        let _ = self.dev.stream_off(self.buf_type);
    }
}

/// The negotiated format: the driver's answer to what was asked.
fn negotiate(
    dev: &VideoDevice,
    buf_type: BufType,
    config: &StreamConfig,
) -> Result<Negotiated, Error> {
    let asked = config.code.bytes();
    let fourcc = styx_lemnos::v4l2::FourCc::new(&asked);
    let not_queue = || Error::Unsupported("the node has no format for its capture queue".into());
    let got = match buf_type {
        BufType::VideoCaptureMplane => {
            let Format::Multi(mut pix) = dev.format(buf_type)? else {
                return Err(not_queue());
            };
            pix.width = config.width;
            pix.height = config.height;
            pix.fourcc = fourcc;
            pix.ycbcr_enc = 0;
            let Format::Multi(got) = dev.set_format(buf_type, &Format::Multi(pix))? else {
                return Err(not_queue());
            };
            let [plane] = got.planes.as_slice() else {
                return Err(Error::Unsupported(format!(
                    "{} planes per frame; one is supported",
                    got.planes.len()
                )));
            };
            Negotiated {
                code: got.fourcc.to_bytes(),
                width: got.width,
                height: got.height,
                bytes_per_line: plane.bytes_per_line,
                size_image: plane.size_image,
            }
        }
        _ => {
            let Format::Single(mut pix) = dev.format(buf_type)? else {
                return Err(not_queue());
            };
            pix.width = config.width;
            pix.height = config.height;
            pix.fourcc = fourcc;
            pix.ycbcr_enc = 0;
            pix.flags &= PREMULTIPLIED_ALPHA;
            let Format::Single(got) = dev.set_format(buf_type, &Format::Single(pix))? else {
                return Err(not_queue());
            };
            Negotiated {
                code: got.fourcc.to_bytes(),
                width: got.width,
                height: got.height,
                bytes_per_line: got.bytes_per_line,
                size_image: got.size_image,
            }
        }
    };
    if got.code != asked || got.width != config.width || got.height != config.height {
        return Err(Error::FormatRefused {
            requested: describe(&asked, config.width, config.height),
            got: describe(&got.code, got.width, got.height),
        });
    }
    Ok(got)
}

fn describe(code: &[u8; 4], width: u32, height: u32) -> String {
    format!("{} {width}x{height}", String::from_utf8_lossy(code))
}

/// A pool whose buffers are the driver's dma-bufs, one per buffer, registered in index order.
fn export_pool(
    dev: &VideoDevice,
    buf_type: BufType,
    lengths: &[usize],
    payload: usize,
) -> Result<FramePool<FrameMetadata>, String> {
    let pool =
        FramePool::<FrameMetadata>::with_capacity(BufferSize::new(payload, 0), lengths.len())
            .map_err(|e| e.to_string())?;
    for (index, &length) in lengths.iter().enumerate() {
        let fd = dev
            .export_buffer_with(buf_type, index as u32, 0, DmaBufAccess::ReadOnly)
            .map_err(|e| e.to_string())?;
        let memory = FdMemory::dmabuf(fd, length, false).map_err(|e| e.to_string())?;
        let at = pool.register(Box::new(memory)).map_err(|e| e.to_string())?;
        if at != index {
            return Err(format!("buffer {index} registered at {at}"));
        }
    }
    Ok(pool)
}

/// A pool of heap buffers for copying frames out of the mapped driver buffers.
fn copied_pool(
    dev: &VideoDevice,
    buf_type: BufType,
    count: usize,
    payload: usize,
) -> Result<(FramePool<FrameMetadata>, Vec<Mapping>), Error> {
    let mut maps = Vec::with_capacity(count);
    for index in 0..count {
        let planes = dev.map_buffer(buf_type, index as u32)?;
        let first = planes
            .into_iter()
            .next()
            .ok_or_else(|| Error::Unsupported(format!("buffer {index} has no plane")))?;
        maps.push(first);
    }
    let pool = FramePool::<FrameMetadata>::with_capacity(BufferSize::new(payload, 0), count)
        .map_err(|e| Error::Unsupported(e.to_string()))?;
    for _ in 0..count {
        pool.register(Box::new(HostMemory::new(payload)))
            .map_err(|e| Error::Unsupported(e.to_string()))?;
    }
    Ok((pool, maps))
}

/// Copies `len` bytes of the mapped buffer into `frame`. `false` when the frame cannot be written.
fn copy_frame(frame: &mut FrameLease<FrameMetadata>, src: &Mapping, len: usize) -> bool {
    let Some(mut planes) = frame.write() else {
        return false;
    };
    let dst = planes.bytes();
    let source = src.as_slice();
    let n = len.min(dst.len()).min(source.len());
    dst[..n].copy_from_slice(&source[..n]);
    record_mmap_copy(n);
    true
}

/// The metadata a frame carries: the controls in force, and what the frame says of itself.
fn metadata(wanted: &[ControlValue; Control::COUNT], stamp: u64) -> FrameMetadata {
    let mut meta = FrameMetadata {
        controls: *wanted,
        sensor_timestamp_ns: stamp,
        ..FrameMetadata::default()
    };
    if let ControlValue::Manual(us) = wanted[Control::Exposure.index()] {
        meta.exposure_us = us;
    }
    if let ControlValue::Manual(kelvin) = wanted[Control::WhiteBalance.index()] {
        meta.colour_temperature_k = Some(kelvin);
    }
    meta
}

/// The wait before reconnect attempt `attempt` (1 for the first): 250 ms doubling, capped.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    (BACKOFF_FIRST * (1u32 << shift.min(31))).min(BACKOFF_CAP)
}

fn duration_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// How the worker's run ended.
enum End {
    Stopped,
    GaveUp(String),
}

/// What one pass over an open session decided.
enum Next {
    Stop,
    Fault(Error),
}

enum Flow {
    Continue,
    /// The consumer dropped its receiver: nothing reads frames any more.
    Gone,
}

/// The worker's state that outlives a session.
struct Env {
    path: PathBuf,
    config: StreamConfig,
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    frames: SyncSender<FrameLease<FrameMetadata>>,
    wanted: [ControlValue; Control::COUNT],
    table: Table,
    devices: Vec<RawControl>,
    attempt: u32,
    last_error: String,
}

fn run(mut env: Env, first: Session) {
    let mut next = Some(first);
    let end = loop {
        let mut session = match next.take() {
            Some(session) => session,
            None => match env.reconnect() {
                Ok(session) => session,
                Err(end) => break end,
            },
        };
        match env.pump(&mut session) {
            Next::Stop => break End::Stopped,
            Next::Fault(error) => {
                env.last_error = error.to_string();
                session.stop();
            }
        }
    };
    env.shared.set_state(match end {
        End::Stopped => State::Stopped,
        End::GaveUp(error) => State::Failed { error },
    });
}

impl Env {
    /// Opens the device again after a fault, with backoff, until it opens or the policy gives up
    /// or the caller stops the stream.
    fn reconnect(&mut self) -> Result<Session, End> {
        loop {
            self.attempt += 1;
            if let Some(limit) = self.config.give_up_after
                && self.attempt > limit
            {
                return Err(End::GaveUp(self.last_error.clone()));
            }
            let wait = backoff(self.attempt);
            self.shared.set_state(State::Retrying {
                attempt: self.attempt,
                backoff: wait,
                error: self.last_error.clone(),
            });
            if self.sleep(wait) {
                return Err(End::Stopped);
            }
            match Session::open(&self.path, &self.config) {
                Ok(session) => {
                    self.table = session.table;
                    self.devices = session.devices.clone();
                    self.reapply(&session);
                    self.attempt = 0;
                    self.shared.reconnects.fetch_add(1, Ordering::Relaxed);
                    self.shared
                        .set(State::Streaming, session.delivery, session.note.clone());
                    return Ok(session);
                }
                Err(error) => self.last_error = error.to_string(),
            }
        }
    }

    /// Sets every mapped control back to the value it was asked for.
    fn reapply(&self, session: &Session) {
        for control in Control::ALL {
            if !self.table.supports(control) {
                continue;
            }
            let value = self.wanted[control.index()];
            if self
                .table
                .apply(&session.dev, session.buf_type, control, value)
                .is_err()
            {
                self.shared.control_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Waits `wait`, answering commands meanwhile. `true` when the caller stopped the stream.
    fn sleep(&mut self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        loop {
            let now = Instant::now();
            if now >= until {
                return false;
            }
            match self.commands.recv_timeout((until - now).min(POLL)) {
                Ok(command) => {
                    if self.handle(command, None) {
                        return true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return true,
            }
        }
    }

    /// Runs one command. `true` when it stops the stream.
    fn handle(&mut self, command: Command, session: Option<&Session>) -> bool {
        match command {
            Command::Stop => true,
            Command::Set {
                control,
                value,
                reply,
            } => {
                let result = self.set(session, control, value);
                let _ = reply.send(result);
                false
            }
            Command::GetDevice { id, reply } => {
                let _ = reply.send(self.get_device(session, id));
                false
            }
            Command::SetDevice { id, value, reply } => {
                let _ = reply.send(self.set_device(session, id, value));
                false
            }
        }
    }

    /// Reads a control by id from the open device.
    fn get_device(&self, session: Option<&Session>, id: u32) -> Result<i64, Error> {
        let control = self.device(id)?;
        if matches!(
            control.control_type,
            ControlType::Button | ControlType::CtrlClass
        ) {
            return Err(Error::DeviceValue(format!("{} has no value", control.name)));
        }
        let session = session.ok_or_else(|| Error::Lost("the device is not open".into()))?;
        session
            .dev
            .control(id)?
            .as_i64()
            .ok_or_else(|| Error::DeviceValue(format!("{} has no integer value", control.name)))
    }

    /// Sets a passthrough control by id on the open device, after checking it can take the value.
    fn set_device(&self, session: Option<&Session>, id: u32, value: i64) -> Result<(), Error> {
        let control = self.device(id)?;
        if let Some(mapped) = control.mapped {
            return Err(Error::DeviceValue(format!(
                "{} is mapped to the Styx control {}: set that one",
                control.name,
                mapped.name()
            )));
        }
        if control.read_only {
            return Err(Error::DeviceValue(format!("{} is read-only", control.name)));
        }
        let raw = match control.control_type {
            ControlType::Button => Raw::Integer(0),
            ControlType::Boolean if value == 0 || value == 1 => Raw::Integer(value as i32),
            ControlType::Boolean => {
                return Err(Error::DeviceValue(format!("{} takes 0 or 1", control.name)));
            }
            ControlType::Integer64 => Raw::Integer64(value),
            ControlType::Integer
            | ControlType::Menu
            | ControlType::IntegerMenu
            | ControlType::Bitmask => {
                let in_range = (control.min..=control.max).contains(&value)
                    || matches!(control.control_type, ControlType::Bitmask);
                if !in_range {
                    return Err(Error::DeviceValue(format!(
                        "{} takes {}..={}, not {value}",
                        control.name, control.min, control.max
                    )));
                }
                Raw::Integer(i32::try_from(value).map_err(|_| {
                    Error::DeviceValue(format!("{} takes 32-bit values", control.name))
                })?)
            }
            _ => {
                return Err(Error::DeviceValue(format!(
                    "{} is a compound control; it is listed, not settable",
                    control.name
                )));
            }
        };
        let session = session.ok_or_else(|| Error::Lost("the device is not open".into()))?;
        session.dev.set_control(id, raw)?;
        Ok(())
    }

    fn device(&self, id: u32) -> Result<&RawControl, Error> {
        self.devices
            .iter()
            .find(|control| control.id == id)
            .ok_or(Error::NoDeviceControl(id))
    }

    /// Sets a control on the open device, or only remembers it while the device is lost.
    fn set(
        &mut self,
        session: Option<&Session>,
        control: Control,
        value: ControlValue,
    ) -> Result<(), Error> {
        match session {
            Some(s) => self.table.apply(&s.dev, s.buf_type, control, value)?,
            None => {
                if !self.table.supports(control) {
                    return Err(Error::NoControl(control));
                }
                control.validate(value)?;
            }
        }
        self.wanted[control.index()] = value;
        Ok(())
    }

    /// Takes frames off an open session until it faults or the stream stops.
    fn pump(&mut self, session: &mut Session) -> Next {
        loop {
            loop {
                match self.commands.try_recv() {
                    Ok(command) => {
                        if self.handle(command, Some(&*session)) {
                            session.stop();
                            return Next::Stop;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        session.stop();
                        return Next::Stop;
                    }
                }
            }
            if let Err(error) = session.requeue_free() {
                return Next::Fault(error);
            }
            match session.dev.wait_readable(Some(POLL)) {
                Ok(ready) if ready.hangup => {
                    return Next::Fault(Error::Lost("the device went away".into()));
                }
                Ok(_) => {}
                Err(error) => return Next::Fault(error.into()),
            }
            loop {
                match session.dev.dequeue(session.buf_type, Memory::Mmap) {
                    Ok(Some(done)) => match self.deliver(session, done) {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::Gone) => {
                            session.stop();
                            return Next::Stop;
                        }
                        Err(error) => return Next::Fault(error),
                    },
                    Ok(None) => break,
                    Err(error) => return Next::Fault(error.into()),
                }
            }
        }
    }

    /// Turns one dequeued buffer into a frame and sends it, or drops it.
    fn deliver(&mut self, session: &mut Session, done: DequeuedBuffer) -> Result<Flow, Error> {
        let index = done.index as usize;
        if index >= session.queued.len() || !session.queued[index] {
            return Err(Error::Lost(format!(
                "buffer {} came back but was not queued",
                done.index
            )));
        }
        session.queued[index] = false;
        let flags = done.flags.bits();
        let used = done.bytes_used();
        let mut stamp = 0u64;
        let mut frame = None;
        if let Some(len) = session.frame_len(used, flags & BUF_ERROR != 0) {
            stamp = if flags & TIMESTAMP_MASK == TIMESTAMP_MONOTONIC {
                duration_ns(done.timestamp)
            } else {
                self.shared.arrival_stamped.fetch_add(1, Ordering::Relaxed);
                duration_ns(monotonic_now()?)
            };
            let entry = session.entry(done.sequence, stamp, len);
            frame = match session.delivery {
                Delivery::Dmabuf => session.pool.acquire_at(index, entry).ok(),
                Delivery::Copied => match session.pool.acquire(entry) {
                    Ok(mut lease) => {
                        copy_frame(&mut lease, &session.maps[index], len).then_some(lease)
                    }
                    Err(_) => None,
                },
            };
        }
        if session.delivery == Delivery::Copied {
            // The driver's buffer goes back at once: the frame is a copy now.
            session.queue(index)?;
        }
        let Some(mut frame) = frame else {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(Flow::Continue);
        };
        if let Some(meta) = frame.meta_mut() {
            *meta = metadata(&self.wanted, stamp);
        }
        match self.frames.try_send(frame) {
            Ok(()) => {
                self.shared.frames.fetch_add(1, Ordering::Relaxed);
                Ok(Flow::Continue)
            }
            Err(TrySendError::Full(_frame)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(Flow::Continue)
            }
            Err(TrySendError::Disconnected(_frame)) => Ok(Flow::Gone),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_a_quarter_second_and_stops_at_five() {
        assert_eq!(backoff(1), Duration::from_millis(250));
        assert_eq!(backoff(2), Duration::from_millis(500));
        assert_eq!(backoff(3), Duration::from_secs(1));
        assert_eq!(backoff(5), Duration::from_secs(4));
        assert_eq!(backoff(6), Duration::from_secs(5));
        assert_eq!(backoff(1_000), Duration::from_secs(5));
        assert_eq!(backoff(0), Duration::from_millis(250));
    }

    #[test]
    fn metadata_reports_the_controls_in_force() {
        let mut wanted = [ControlValue::Auto; Control::COUNT];
        wanted[Control::Exposure.index()] = ControlValue::Manual(12_000);
        wanted[Control::WhiteBalance.index()] = ControlValue::Manual(4_500);
        let meta = metadata(&wanted, 42);
        assert_eq!(meta.sensor_timestamp_ns, 42);
        assert_eq!(meta.exposure_us, 12_000);
        assert_eq!(meta.colour_temperature_k, Some(4_500));
        assert_eq!(meta.control(Control::Gain), ControlValue::Auto);
    }

    #[test]
    fn a_format_the_driver_changed_says_both_sides() {
        assert_eq!(describe(b"YUYV", 640, 480), "YUYV 640x480");
    }
}
