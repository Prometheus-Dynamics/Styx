//! Styx's client for Lemnos cameras, over the `lemnos-client` socket.
//!
//! [`find_chain`] finds the `camera.sensor/1` device by tag and follows `link` to its receiver and,
//! when it has one, its ISP; [`find_chains`] does that for every sensor on the board. [`Frames`]
//! opens the receiver's frame stream and turns each of its buffers into [`styx_frame`] buffers,
//! once, so that every frame is a [`FrameLease`] over the memory Lemnos filled: no copy. A lease's
//! id is the frame's sequence in its stream; its timestamp is the frame's timestamp, unchanged.
//! [`open_job_stream`] is the writable form, for a job stream the consumer fills and posts on.
//!
//! A `Frames` or `JobStream` that drops closes its stream on the daemon, so the stream opens again.
//!
//! Lemnos does no 3A and holds no tuning. This crate is discovery and buffers only.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use lemnos_client::{Client, DeviceEntry, FrameStream, HeldFrame, StreamPlatform, follow};
use lemnos_client::{Next, ProducerState};
use lemnos_linux_stream::LinuxStreams;
use styx_format::{FourCc, MediaFormat};
use styx_frame::{
    AcquireError, BufferMemory, BufferSize, FdMemory, FrameEntry, FrameLease, FramePool, Layouts,
    PlaneLayout, PoolError, PoolStats,
};

/// ISP600 layouts through the vendor vin driver (feature `isp600`).
#[cfg(feature = "isp600")]
pub use lemnos_driver_isp600 as isp600;
/// ISP8000 layouts behind the vendor vvcam device (feature `k230isp`).
#[cfg(feature = "k230isp")]
pub use lemnos_driver_k230isp as k230isp;
/// PiSP layouts and tiling, without the `device` feature (feature `pisp`).
#[cfg(feature = "pisp")]
pub use lemnos_driver_pisp as pisp;
/// Rockchip ISP layouts, the vendor kernel driver's stock nodes (feature `rkisp`).
#[cfg(feature = "rkisp")]
pub use lemnos_driver_rkisp as rkisp;
/// Lemnos's V4L2 library: nodes, subdevices, the media controller (feature `v4l2`).
#[cfg(feature = "v4l2")]
pub use lemnos_v4l2 as v4l2;

/// The tag of a sensor device.
pub const SENSOR_TAG: &str = "camera.sensor/1";
/// The tag of a receiver device.
pub const RECEIVER_TAG: &str = "camera.receiver/1";
/// The tag of an ISP device.
pub const ISP_TAG: &str = "camera.isp/1";
/// The tag of a device that delivers finished frames itself (USB).
pub const FRAMES_TAG: &str = "camera.frames/1";

/// Connects to the Lemnos service at `path` as `name`.
pub fn connect(path: impl AsRef<std::path::Path>, name: &str) -> Result<Client, Error> {
    Client::connect(path, name).map_err(Error::from_client)
}

/// A camera's devices, found by tag and link.
///
/// The sensor's `link` names its receiver. The receiver's `link` names its first ISP, and each ISP's
/// `link` the next, so `isps` is the ordered list of ISPs the frames pass through. A receiver with
/// no link is a camera with no ISP, which is valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub sensor: DeviceEntry,
    pub receiver: DeviceEntry,
    /// The ISPs after the receiver, in link order. Empty when the receiver links to none.
    pub isps: Vec<DeviceEntry>,
}

/// Why no camera chain was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// No device carries `camera.sensor/1`.
    NoSensor,
    /// More than one does; the caller must pick one by name.
    SensorCount(usize),
    /// The sensor's `link` is empty or names no device.
    NotLinked(String),
    /// The sensor's link does not name a device with `camera.receiver/1` (an ISP linked straight
    /// from a sensor is this too), or a link after the receiver names something that is not an
    /// ISP.
    NotReceiver { device: String, tags: Vec<String> },
    /// A link after the receiver names a device that is not `camera.isp/1`.
    NotIsp { device: String, tags: Vec<String> },
    /// A device's `link` names no device on the board.
    Dangling { from: String, to: String },
    /// The links go round in a circle: `path` is the devices from the first one repeated, in link
    /// order, ending with the one that closes the circle.
    Cycle { path: Vec<String> },
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSensor => write!(f, "no {SENSOR_TAG} device"),
            Self::SensorCount(n) => write!(f, "{n} {SENSOR_TAG} devices; pick one by name"),
            Self::NotLinked(from) => write!(f, "{from}: no link to a receiver"),
            Self::NotReceiver { device, tags } => {
                write!(f, "{device}: not a {RECEIVER_TAG} device (tags {tags:?})")
            }
            Self::NotIsp { device, tags } => {
                write!(f, "{device}: not a {ISP_TAG} device (tags {tags:?})")
            }
            Self::Dangling { from, to } => {
                write!(f, "{from}: links to {to}, which is not on the board")
            }
            Self::Cycle { path } => write!(f, "links go round in a circle: {}", path.join(" -> ")),
        }
    }
}

impl std::error::Error for ChainError {}

fn has_tag(device: &DeviceEntry, tag: &str) -> bool {
    device.tags.iter().any(|t| t == tag)
}

/// The sensors in `devices`, in listing order.
fn sensors<'a>(devices: &'a [DeviceEntry]) -> impl Iterator<Item = &'a DeviceEntry> + 'a {
    devices.iter().filter(|d| has_tag(d, SENSOR_TAG))
}

fn named<'a>(devices: &'a [DeviceEntry], name: &str) -> Option<&'a DeviceEntry> {
    devices.iter().find(|d| d.name == name)
}

/// The chain that starts at `sensor`, following `link` through `devices` until a device has none.
///
/// Every hop is checked against its contract tag: the sensor's link must reach a receiver, and each
/// link after the receiver must reach an ISP. A link that names no device, or that comes back to a
/// device already passed, is an error that names the devices.
fn chain_from(devices: &[DeviceEntry], sensor: &DeviceEntry) -> Result<Chain, ChainError> {
    let receiver = match sensor.link.as_deref().and_then(|to| named(devices, to)) {
        Some(device) => device,
        None => return Err(ChainError::NotLinked(sensor.name.clone())),
    };
    if !has_tag(receiver, RECEIVER_TAG) {
        return Err(ChainError::NotReceiver {
            device: receiver.name.clone(),
            tags: receiver.tags.clone(),
        });
    }
    let mut path: Vec<&DeviceEntry> = vec![sensor, receiver];
    let mut isps = Vec::new();
    let mut at = receiver;
    while let Some(to) = at.link.as_deref() {
        let Some(next) = named(devices, to) else {
            return Err(ChainError::Dangling {
                from: at.name.clone(),
                to: to.to_string(),
            });
        };
        if let Some(start) = path.iter().position(|seen| seen.name == next.name) {
            let mut cycle: Vec<String> = path[start..].iter().map(|d| d.name.clone()).collect();
            cycle.push(next.name.clone());
            return Err(ChainError::Cycle { path: cycle });
        }
        if !has_tag(next, ISP_TAG) {
            return Err(ChainError::NotIsp {
                device: next.name.clone(),
                tags: next.tags.clone(),
            });
        }
        isps.push(next.clone());
        path.push(next);
        at = next;
    }
    Ok(Chain {
        sensor: sensor.clone(),
        receiver: receiver.clone(),
        isps,
    })
}

/// Finds the one sensor in `devices`, then its receiver and ISP by following `link`.
///
/// A sensor with no receiver is refused. A receiver with no link is a camera without an ISP,
/// which is valid. More than one sensor is [`ChainError::SensorCount`]: see [`find_chains`].
pub fn find_chain(devices: &[DeviceEntry]) -> Result<Chain, ChainError> {
    let mut found = sensors(devices);
    match (found.next(), found.next()) {
        (None, _) => Err(ChainError::NoSensor),
        (Some(one), None) => chain_from(devices, one),
        (Some(_), Some(_)) => Err(ChainError::SensorCount(sensors(devices).count())),
    }
}

/// Every sensor in `devices`, in listing order, each with its chain or why it is not one.
///
/// One entry per `camera.sensor/1` device, so a board with two sensors gives two entries. Each
/// chain is followed through the whole list, so a receiver shared by sensors resolves for each.
pub fn find_chains(devices: &[DeviceEntry]) -> Vec<Result<Chain, ChainError>> {
    sensors(devices)
        .map(|sensor| chain_from(devices, sensor))
        .collect()
}

/// A shared structure of a frame stream whose layout value is not the one this build reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedLayout {
    /// The frame ring's header.
    Ring,
    /// A reader's marks page.
    Marks,
}

impl SharedLayout {
    /// The structure as a person reads it: the start of the refusal's message.
    #[must_use]
    pub const fn what(self) -> &'static str {
        match self {
            Self::Ring => "this ring",
            Self::Marks => "this marks page",
        }
    }
}

/// A layout refusal: the structure, the value in the memory, and the value this build reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRefusal {
    /// The structure whose layout is refused.
    pub which: SharedLayout,
    /// The value in the memory.
    pub found: u64,
    /// The value this build reads.
    pub expected: u64,
}

impl fmt::Display for LayoutRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is layout {}; this build reads layout {}",
            self.which.what(),
            self.found,
            self.expected
        )
    }
}

/// Why a frame stream ended, as the host wrote its end word. Three kinds are read as three outcomes
/// and an unknown code as a fourth: a stream is never read as running after any end word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// The host ended the stream: a stop, a close, or the host going away.
    Closed,
    /// The stream ended because this reader broke the rules. That is a bug in Styx, never a camera
    /// fault: the reader is told so, and its status says so.
    ConsumerFault,
    /// The stream ended because the device producing it failed for good (its auto-off policy).
    DeviceFailed,
    /// An end code this build does not know: the stream ended, its kind unknown.
    Unknown(u32),
}

impl From<lemnos_client::StreamEnd> for StreamEnd {
    fn from(end: lemnos_client::StreamEnd) -> Self {
        match end {
            lemnos_client::StreamEnd::Closed => Self::Closed,
            lemnos_client::StreamEnd::ConsumerFault => Self::ConsumerFault,
            lemnos_client::StreamEnd::DeviceFailed => Self::DeviceFailed,
            lemnos_client::StreamEnd::Unknown(code) => Self::Unknown(code),
        }
    }
}

impl fmt::Display for StreamEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("the host ended the frame stream"),
            Self::ConsumerFault => f.write_str(
                "the frame stream ended because this reader broke the rules: a bug in Styx",
            ),
            Self::DeviceFailed => {
                f.write_str("the frame stream ended: the device producing it failed")
            }
            Self::Unknown(code) => {
                write!(f, "the frame stream ended, kind unknown (end code {code})")
            }
        }
    }
}

/// Whether the camera kept what it was given through a stall. `Kept`: the same device instance
/// recovered and the frames continue (a gap in time, not a restart). `Lost`: the host bound the
/// device again as a new instance, so its mode and controls must be set again, and the frame
/// sequence restarts: a discontinuity, not a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carried {
    Kept,
    Lost,
}

impl From<lemnos_client::Carried> for Carried {
    fn from(carried: lemnos_client::Carried) -> Self {
        match carried {
            lemnos_client::Carried::Kept => Self::Kept,
            lemnos_client::Carried::Lost => Self::Lost,
        }
    }
}

/// What a wait for the next event on a frame stream found.
#[derive(Debug)]
pub enum Event<M = ()> {
    /// A frame, as a lease over its buffer.
    Frame(FrameLease<M>),
    /// The time passed with the stream running and no frame.
    TimedOut,
    /// The camera failed and is being retried since `since_ns` (CLOCK_MONOTONIC nanoseconds; 0
    /// when the host did not say). Once for each failure the reader sees, before the frames after
    /// it. The stream stays attached: this is health, not an end.
    Stalled { since_ns: u64 },
    /// The camera is back, once after a `Stalled` and before the first frame after it.
    Resumed(Carried),
    /// The stream ended and every frame it had is read.
    Ended(StreamEnd),
}

/// Errors from opening and reading frame streams.
#[derive(Debug)]
pub enum Error {
    Client(lemnos_client::Error),
    /// A shared ring or marks page of the stream is another layout than this build reads. Only
    /// that stream is affected; nothing was mapped.
    SharedLayout(LayoutRefusal),
    /// This Styx and the board's Lemnos are different builds: a base method of contract `contract`
    /// is laid out differently on the device than this client was built with (a base hash
    /// differs). Refused at bind; nothing was called. `source` is the client's error in full, so
    /// its words (both hashes) survive.
    ContractMismatch {
        contract: String,
        ours: u64,
        theirs: u64,
        source: lemnos_client::Error,
    },
    /// The stream ended and no frame is left to read: why, as the host wrote it. No more frames
    /// come from this stream; a session reopens it.
    StreamEnded(StreamEnd),
    Chain(ChainError),
    /// The stream's buffers could not be mapped.
    Map(io::Error),
    /// The stream has no buffers this crate can map (neither one memfd nor one dma-buf each).
    NoBuffers,
    /// The stream has this many planes per buffer; this crate maps one plane per buffer.
    Planes(u32),
    /// A frame names a buffer outside the stream.
    BadBuffer(u32),
    /// A frame named a buffer whose lease is still out; the frame was given back to Lemnos.
    Held(u32),
    /// A compressed frame is longer than its buffer's payload; the frame was given back.
    Oversize {
        len: usize,
        payload: usize,
    },
    Pool(PoolError),
    Acquire(AcquireError),
    Layout(styx_format::LayoutError),
    /// The stride the geometry names is shorter than one row of pixels.
    Stride {
        stride: u32,
        min: usize,
    },
    /// The geometry's frame (`stride` times `height`) is larger than a buffer's payload.
    FrameTooLarge {
        size: usize,
        payload: usize,
    },
    /// The geometry names a format that is not one plane of raw pixels (a compressed or
    /// multi-plane format needs its own layout).
    NotSinglePlane(FourCc),
}

impl Error {
    /// The Lemnos client's error as Styx's. A shared-layout refusal keeps its structure and both
    /// values as [`Error::SharedLayout`]; every other client error is [`Error::Client`] unchanged.
    pub fn from_client(error: lemnos_client::Error) -> Self {
        if let lemnos_client::Error::ContractMismatch {
            contract,
            ours,
            theirs,
        } = &error
        {
            return Self::ContractMismatch {
                contract: contract.clone(),
                ours: *ours,
                theirs: *theirs,
                source: error.clone(),
            };
        }
        match error {
            lemnos_client::Error::Stream(lemnos_wire::ring::RingError::Layout {
                which,
                found,
                expected,
            }) => Self::SharedLayout(LayoutRefusal {
                which: match which {
                    lemnos_wire::ring::SharedLayout::Ring => SharedLayout::Ring,
                    lemnos_wire::ring::SharedLayout::Marks => SharedLayout::Marks,
                },
                found,
                expected,
            }),
            other => Self::Client(other),
        }
    }

    /// The layout refusal this error is, if it is one.
    #[must_use]
    pub fn layout_refusal(&self) -> Option<LayoutRefusal> {
        match self {
            Self::SharedLayout(refusal) => Some(*refusal),
            _ => None,
        }
    }

    /// The contract whose base method differs from this build's, if this is a contract mismatch.
    #[must_use]
    pub fn contract_mismatch(&self) -> Option<&str> {
        match self {
            Self::ContractMismatch { contract, .. } => Some(contract),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(e) => write!(f, "lemnos client: {e}"),
            Self::SharedLayout(refusal) => write!(f, "frame stream: {refusal}"),
            Self::ContractMismatch { source, .. } => write!(
                f,
                "this Styx and the board's Lemnos are different builds: {source}"
            ),
            Self::StreamEnded(end) => write!(f, "{end}"),
            Self::Chain(e) => write!(f, "{e}"),
            Self::Map(e) => write!(f, "mapping stream buffers: {e}"),
            Self::NoBuffers => write!(f, "stream has no mappable buffers"),
            Self::Planes(n) => write!(f, "stream has {n} planes per buffer; one is mapped here"),
            Self::BadBuffer(b) => write!(f, "frame names buffer {b}, which is out of range"),
            Self::Held(b) => write!(f, "buffer {b} still held; frame dropped"),
            Self::Oversize { len, payload } => {
                write!(f, "compressed frame of {len} bytes; buffers hold {payload}")
            }
            Self::Pool(e) => write!(f, "frame pool: {e:?}"),
            Self::Acquire(e) => write!(f, "frame acquire: {e:?}"),
            Self::Layout(e) => write!(f, "frame layout: {e:?}"),
            Self::Stride { stride, min } => {
                write!(f, "stride {stride} is shorter than a row of {min} bytes")
            }
            Self::FrameTooLarge { size, payload } => {
                write!(f, "frame of {size} bytes; buffers hold {payload}")
            }
            Self::NotSinglePlane(code) => write!(f, "{code:?} is not one plane of raw pixels"),
        }
    }
}

impl std::error::Error for Error {}

/// A stream taken from the daemon. It is given back when this drops, or at [`Taken::close`] with
/// the daemon's answer. A stream left open holds its name on the daemon until the connection ends,
/// so the next open of the same stream would wait on it.
struct Taken {
    client: Client,
    stream: Option<FrameStream>,
}

impl Taken {
    fn stream(&self) -> &FrameStream {
        self.stream
            .as_ref()
            .expect("a taken stream is held until it is closed or taken")
    }

    fn stream_mut(&mut self) -> &mut FrameStream {
        self.stream
            .as_mut()
            .expect("a taken stream is held until it is closed or taken")
    }

    /// Gives the stream back to the daemon and returns its answer.
    fn close(mut self) -> Result<(), Error> {
        match self.stream.take() {
            Some(stream) => self.client.close_stream(stream).map_err(Error::from_client),
            None => Ok(()),
        }
    }

    /// Hands the stream to the caller, who must give it back (`Client::close_stream`).
    fn into_stream(mut self) -> FrameStream {
        self.stream
            .take()
            .expect("a taken stream is held until it is closed or taken")
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            // The daemon's answer has nowhere to go from here; a failure leaves the stream to the
            // connection's end, as it would have without this.
            let _ = self.client.close_stream(stream);
        }
    }
}

impl fmt::Debug for Taken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Taken")
            .field("stream", &self.stream.as_ref().map(|s| s.name()))
            .finish()
    }
}

/// Opens `stream` on `device` and maps each of its buffers into one pool. The stream is taken as
/// its writable or read-only form; on any error after it is taken it is given back.
fn open_mapped<M: Default + Send + Sync>(
    client: &Client,
    device: u32,
    stream: &str,
    writable: bool,
) -> Result<(Taken, FramePool<M>), Error> {
    let platform: Arc<dyn StreamPlatform> = Arc::new(LinuxStreams);
    let opened = if writable {
        client.open_stream_writable(device, stream, platform)
    } else {
        client.open_stream(device, stream, platform)
    }
    .map_err(Error::from_client)?;
    // From here the stream is given back on every error path: `Taken` does it on drop.
    let taken = Taken {
        client: client.clone(),
        stream: Some(opened),
    };
    let memories = buffer_memories(taken.stream(), writable)?;
    if memories.is_empty() {
        return Err(Error::NoBuffers);
    }
    let pool = pool_over(memories, taken.stream().buffer_len())?;
    Ok((taken, pool))
}

/// A frame stream's geometry: its format, its size, and the bytes per line the receiver writes
/// (padding included). The receiver's `layout` gives all of it; `stride` is ignored for a
/// compressed format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub code: FourCc,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Geometry {
    /// The geometry of a packed frame of `code` at `width` x `height`: rows as tight as the format
    /// allows. For streams whose rows nobody pads (statistics, a producer's own packing).
    pub fn packed(code: FourCc, width: u32, height: u32) -> Result<Self, Error> {
        if code.is_compressed() {
            return Ok(Self {
                code,
                width,
                height,
                stride: 0,
            });
        }
        let row = styx_format::plane_sizes(code, width, height, None, None)
            .map_err(Error::Layout)?
            .first()
            .map(|plane| plane.stride)
            .ok_or(Error::NotSinglePlane(code))?;
        let stride =
            u32::try_from(row).map_err(|_| Error::Layout(styx_format::LayoutError::Overflow))?;
        Ok(Self {
            code,
            width,
            height,
            stride,
        })
    }
}

/// The plane layout of a geometry: one plane of `stride` bytes per line, `height` lines. Refused
/// for a stride shorter than a row, and for a format that is not one plane of raw pixels.
fn geometry_layouts(geometry: Geometry) -> Result<Option<Layouts>, Error> {
    if geometry.code.is_compressed() {
        return Ok(None);
    }
    let planes =
        styx_format::plane_sizes(geometry.code, geometry.width, geometry.height, None, None)
            .map_err(Error::Layout)?;
    if planes.len() != 1 {
        return Err(Error::NotSinglePlane(geometry.code));
    }
    let min = planes[0].stride;
    let stride = geometry.stride as usize;
    if stride < min {
        return Err(Error::Stride {
            stride: geometry.stride,
            min,
        });
    }
    let len = stride
        .checked_mul(geometry.height as usize)
        .ok_or(Error::Layout(styx_format::LayoutError::Overflow))?;
    Layouts::new(&[PlaneLayout::new(0, stride, len)])
        .map(Some)
        .ok_or(Error::Layout(styx_format::LayoutError::Overflow))
}

/// Opens `stream` on `device` and maps its buffers, as one pool. Frames are `code` at `width` x
/// `height`, packed (see [`Geometry::packed`]). Each buffer's metadata is `M`, made once by the pool
/// (`M::default()`).
///
/// The buffer count and size are the stream's own, from the open reply ([`FrameStream::layout`]).
/// Dropping the returned [`Frames`] gives the stream back to the daemon.
pub fn open_frames<M: Default + Send + Sync>(
    client: &Client,
    device: u32,
    stream: &str,
    code: FourCc,
    width: u32,
    height: u32,
) -> Result<Frames<M>, Error> {
    let geometry = Geometry::packed(code, width, height)?;
    open_frames_at(client, device, stream, geometry)
}

/// [`open_frames`] for a stream whose geometry the receiver gives: each lease's plane is
/// `geometry.stride` bytes per line, so row `r` starts at `r * stride` in the buffer.
pub fn open_frames_at<M: Default + Send + Sync>(
    client: &Client,
    device: u32,
    stream: &str,
    geometry: Geometry,
) -> Result<Frames<M>, Error> {
    open_frames_with(client, device, stream, geometry, false)
}

/// [`open_frames_at`] with the buffers mapped for writing too: for a producer in this process that
/// fills the stream's buffers (a simulated camera, a software stage writing in place).
pub fn open_frames_writable_at<M: Default + Send + Sync>(
    client: &Client,
    device: u32,
    stream: &str,
    geometry: Geometry,
) -> Result<Frames<M>, Error> {
    open_frames_with(client, device, stream, geometry, true)
}

/// [`open_frames`] with the buffers mapped for writing too (see [`open_frames_writable_at`]).
pub fn open_frames_writable<M: Default + Send + Sync>(
    client: &Client,
    device: u32,
    stream: &str,
    code: FourCc,
    width: u32,
    height: u32,
) -> Result<Frames<M>, Error> {
    let geometry = Geometry::packed(code, width, height)?;
    open_frames_writable_at(client, device, stream, geometry)
}

fn open_frames_with<M: Default + Send + Sync>(
    client: &Client,
    device: u32,
    stream: &str,
    geometry: Geometry,
    writable: bool,
) -> Result<Frames<M>, Error> {
    let (taken, pool) = open_mapped::<M>(client, device, stream, writable)?;
    let format = MediaFormat::with_defaults(
        geometry.code,
        styx_format::Resolution::new(geometry.width, geometry.height).ok_or(Error::NoBuffers)?,
    );
    // A compressed stream's frames are one plane each, sized per frame (see `lease_for`).
    let layouts = geometry_layouts(geometry)?;
    if let Some(layouts) = layouts {
        let size = layouts.span().unwrap_or(0);
        let payload = pool.buffer_size().payload;
        if size > payload {
            return Err(Error::FrameTooLarge { size, payload });
        }
    }
    let count = pool.buffers();
    Ok(Frames {
        taken,
        pool,
        pending: (0..count).map(|_| None).collect(),
        format,
        layouts,
        held_drops: 0,
        oversize_drops: 0,
        by_job: false,
    })
}

/// Opens `stream` on `device` for a job stream: the consumer writes the stream's buffers and posts
/// jobs on it, so it is mapped writable. Its buffers are one pool, and each buffer's metadata is
/// `M` (made once by the pool).
///
/// Give it back with [`JobStream::close`], or drop it.
pub fn open_job_stream<M: Default + Send + Sync>(
    client: &Client,
    device: u32,
    stream: &str,
) -> Result<JobStream<M>, Error> {
    let (taken, pool) = open_mapped::<M>(client, device, stream, true)?;
    Ok(JobStream { taken, pool })
}

/// One memory per buffer, each mapped from the region the stream gives for it: a shared pool's
/// memfd at the buffer's offset, or the buffer's own dma-buf. Nothing is copied: the frame pool
/// and the stream's own mapping are two views of the same memory.
///
/// A stream of several planes per buffer is refused: this crate's pool holds one memory per
/// buffer. `writable` maps the buffers read-write; a read-only stream maps them read-only.
pub fn buffer_memories(
    stream: &FrameStream,
    writable: bool,
) -> Result<Vec<Box<dyn BufferMemory>>, Error> {
    let planes = stream.planes().count();
    if planes != 1 {
        return Err(Error::Planes(planes));
    }
    let buffer_bytes = stream.buffer_len();
    (0..stream.buffer_count())
        .map(|buffer| {
            let region = stream.region(buffer, 0).ok_or(Error::NoBuffers)?;
            let len = usize::try_from(region.payload_bytes).map_err(|_| Error::NoBuffers)?;
            if len != buffer_bytes {
                return Err(Error::NoBuffers);
            }
            let offset = usize::try_from(region.offset).map_err(|_| Error::NoBuffers)?;
            let fd = region.fd.try_clone_to_owned().map_err(Error::Map)?;
            let mapped = match region.kind {
                lemnos_client::MemoryKind::SharedMemory => {
                    FdMemory::memfd_at(fd, offset, len, writable)
                }
                lemnos_client::MemoryKind::DmaBuf => FdMemory::dmabuf_at(fd, offset, len, writable),
            }
            .map_err(Error::Map)?;
            Ok(Box::new(mapped) as Box<dyn BufferMemory>)
        })
        .collect()
}

/// One pool over all of a stream's buffers. Frames are acquired with
/// [`FramePool::acquire_at`], at the buffer index the ring names.
pub fn pool_over<M: Default + Send + Sync>(
    memories: Vec<Box<dyn BufferMemory>>,
    buffer_bytes: usize,
) -> Result<FramePool<M>, Error> {
    let size = BufferSize {
        payload: buffer_bytes,
        gpu_slack: 0,
    };
    FramePool::new(size, memories).map_err(Error::Pool)
}

/// A frame's time on `CLOCK_MONOTONIC`. Lemnos's 0 means the producer had no time for the frame,
/// so it is no time at all, and not time zero: the frame is stamped on arrival, as `styx-frame`
/// stamps every source that has no clock of its own.
fn frame_time(frame: &HeldFrame) -> u64 {
    match frame.timestamp_ns() {
        0 => monotonic_ns(),
        time => time,
    }
}

fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer for the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// A frame stream's buffers as one frame pool, and the stream it came from.
///
/// A buffer goes back to Lemnos only when no lease on it is out: [`Frames::next`] first gives back
/// every buffer whose lease has dropped, then waits for a frame. A frame that names a buffer whose
/// lease is still out is counted in [`Frames::held_drops`] and given back at once, never waited on.
///
/// A compressed stream (`FourCc::is_compressed`) has no fixed layout: each lease is one plane at
/// offset 0 whose length is that frame's `HeldFrame::len()`, so its bytes are the bitstream.
///
/// Dropping a `Frames` closes its stream on the daemon. [`Frames::close`] does the same and
/// returns the daemon's answer.
pub struct Frames<M = ()> {
    taken: Taken,
    pool: FramePool<M>,
    /// The Lemnos frame of each buffer, until its lease has dropped.
    pending: Vec<Option<HeldFrame>>,
    format: MediaFormat,
    /// The plane layout of an uncompressed stream; `None` for a compressed one.
    layouts: Option<Layouts>,
    held_drops: u64,
    oversize_drops: u64,
    /// Number each frame by the job that made it, not by its place in the stream.
    by_job: bool,
}

impl<M> fmt::Debug for Frames<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frames")
            .field("stream", &self.taken.stream().name())
            .field("buffers", &self.pool.buffers())
            .field("held_drops", &self.held_drops)
            .finish()
    }
}

impl<M> Frames<M> {
    /// Numbers each frame by the job that made it: the frame id is the identity the job was
    /// posted with, so a result can be matched to its job. For a stream of job results
    /// (statistics, a device's outputs). A capture stream keeps the default, the frame's
    /// sequence in its stream: nothing posts jobs to it, and its identity is 0 on every frame.
    #[must_use]
    pub fn numbered_by_job(mut self) -> Self {
        self.by_job = true;
        self
    }

    /// Buffers in the stream.
    pub fn buffers(&self) -> usize {
        self.pool.buffers()
    }

    /// The stream's frame pool, borrowed. Its buffers are fd-backed (one memfd at offsets, or a
    /// dma-buf each), so a producer can publish them with `styx_socket::Producer::with_pool`
    /// (it takes a handle with `FramePool::clone`).
    pub fn pool(&self) -> &FramePool<M> {
        &self.pool
    }

    /// Frames the reader was too late for, in total: overwritten or taken back by the producer before
    /// [`Frames::next`] or [`Frames::try_next`] read them. Lemnos counts them as `skipped`; they are
    /// not frames of this crate's pool and never reach a lease.
    pub fn skipped(&self) -> u64 {
        self.taken.stream().skipped()
    }

    /// The pool generation of the Lemnos stream this was mapped from. A new generation means the
    /// daemon made a new pool: this `Frames` is then on a pool that is gone and must be reopened.
    pub fn generation(&self) -> u32 {
        self.taken.stream().generation()
    }

    /// Frames dropped because their buffer's previous lease was still out.
    pub fn held_drops(&self) -> u64 {
        self.held_drops
    }

    /// Compressed frames dropped because they were longer than a buffer's payload.
    pub fn oversize_drops(&self) -> u64 {
        self.oversize_drops
    }

    /// The frame pool's counts: leases acquired, acquires that found a buffer held.
    pub fn stats(&self) -> PoolStats {
        self.pool.stats()
    }

    /// Gives back to Lemnos every buffer whose lease has dropped: a buffer with a frame pending
    /// and no lease on it is free.
    fn sweep(&mut self) -> Result<(), Error> {
        for index in 0..self.pending.len() {
            if !self.pool.is_free(index) {
                continue;
            }
            if let Some(frame) = self.pending[index].take() {
                self.taken
                    .stream_mut()
                    .release(frame)
                    .map_err(Error::from_client)?;
            }
        }
        Ok(())
    }

    /// Waits up to `timeout` for the next event: a frame, the time running out, the producer's
    /// stall or its return (each once), or the end once every frame is read. See [`Event`].
    ///
    /// # Errors
    ///
    /// The frame's own errors ([`Error::Held`], [`Error::Oversize`], ...) and a stream's refused
    /// reads; an end is an event, not an error.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Event<M>, Error> {
        self.sweep()?;
        let next = self
            .taken
            .stream_mut()
            .next_or_end(timeout)
            .map_err(Error::from_client)?;
        match next {
            Next::Frame(frame) => self.lease_for(frame).map(Event::Frame),
            Next::TimedOut => Ok(Event::TimedOut),
            Next::Stalled(state) => Ok(Event::Stalled {
                since_ns: match state {
                    ProducerState::Retrying { since_ns } => since_ns,
                    ProducerState::Running | ProducerState::Unknown(_) => 0,
                },
            }),
            Next::Resumed { state } => Ok(Event::Resumed(state.into())),
            Next::Ended(end) => Ok(Event::Ended(end.into())),
        }
    }

    /// Waits up to `timeout` for a frame and returns it as a lease over its buffer. A stall and its
    /// return are not frames: the wait goes on past them, so a caller that wants them uses
    /// [`Frames::next_event`].
    ///
    /// # Errors
    ///
    /// [`Error::StreamEnded`] with the host's reason once the stream has ended and no frame is left:
    /// the loop that calls this ends there, rather than waiting on a stream that will not produce.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<FrameLease<M>>, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next_event(left)? {
                Event::Frame(lease) => return Ok(Some(lease)),
                Event::TimedOut => return Ok(None),
                Event::Stalled { .. } | Event::Resumed(_) => {}
                Event::Ended(end) => return Err(Error::StreamEnded(end)),
            }
        }
    }

    /// Takes a frame if one is ready, without waiting.
    ///
    /// # Errors
    ///
    /// [`Error::StreamEnded`] as [`Frames::next`] gives it.
    pub fn try_next(&mut self) -> Result<Option<FrameLease<M>>, Error> {
        self.sweep()?;
        let Some(frame) = self
            .taken
            .stream_mut()
            .try_next()
            .map_err(Error::from_client)?
        else {
            return self.ended_or_none();
        };
        self.lease_for(frame).map(Some)
    }

    /// No frame was pending: `None` while the stream runs, [`Error::StreamEnded`] with the host's
    /// reason once it has ended.
    fn ended_or_none<T>(&self) -> Result<Option<T>, Error> {
        match self.taken.stream().end() {
            None => Ok(None),
            Some(end) => Err(Error::StreamEnded(end.into())),
        }
    }

    fn lease_for(&mut self, frame: HeldFrame) -> Result<FrameLease<M>, Error> {
        let index = frame.buffer() as usize;
        let layouts = match self.layouts {
            Some(layouts) => layouts,
            None => {
                let payload = self.pool.buffer_size().payload;
                if frame.len() > payload {
                    let len = frame.len();
                    self.oversize_drops += 1;
                    self.taken
                        .stream_mut()
                        .release(frame)
                        .map_err(Error::from_client)?;
                    return Err(Error::Oversize { len, payload });
                }
                // The frame's one plane: the bytes it holds, as the stream's own arithmetic counts them.
                let len = self.taken.stream().plane_len(&frame, 0);
                Layouts::new(&[PlaneLayout::new(0, 0, len)])
                    .ok_or(Error::Layout(styx_format::LayoutError::Overflow))?
            }
        };
        // The frame's number in its stream. Lemnos's `identity` is the consumer's own tag for the
        // values it posted, and is 0 on every frame until something posts one, so it cannot
        // number captured frames. A stream of job results is numbered by it on request.
        let id = if self.by_job {
            frame.identity()
        } else {
            frame.sequence()
        };
        let entry = FrameEntry::new(id, frame_time(&frame), self.format, layouts);
        match self.pool.acquire_at(index, entry) {
            Ok(lease) => {
                self.pending[index] = Some(frame);
                Ok(lease)
            }
            Err(AcquireError::Held { .. }) => {
                self.held_drops += 1;
                self.taken
                    .stream_mut()
                    .release(frame)
                    .map_err(Error::from_client)?;
                Err(Error::Held(index as u32))
            }
            Err(e) => {
                self.taken
                    .stream_mut()
                    .release(frame)
                    .map_err(Error::from_client)?;
                Err(Error::Acquire(e))
            }
        }
    }

    /// Ends a lease. The buffer goes back to Lemnos on a later call, once the lease has dropped.
    pub fn release(&mut self, lease: FrameLease<M>) {
        drop(lease);
    }

    /// Closes the stream on the daemon, so it can be opened again, and returns the daemon's
    /// answer. Dropping the `Frames` does the same without the answer.
    pub fn close(self) -> Result<(), Error> {
        self.taken.close()
    }

    /// Hands the stream to the caller, who must close it with `Client::close_stream`. The frames
    /// are not closed here.
    pub fn into_stream(self) -> FrameStream {
        self.taken.into_stream()
    }
}

/// A writable job stream: one stream whose buffers the consumer fills and whose jobs it posts.
///
/// The buffers are one [`FramePool`]. The consumer takes a buffer by number with
/// [`FramePool::acquire_at`] (the work it writes, the result the job fills), posts the job with
/// [`JobStream::post`], and waits for the completion frame with [`JobStream::wait`]. A completion
/// names its buffer with [`HeldFrame::buffer`], and its bytes are read with [`JobStream::bytes`].
/// The frame is given back with [`JobStream::release`].
///
/// Dropping a `JobStream` closes its stream on the daemon, as [`Frames`] does.
pub struct JobStream<M = ()> {
    taken: Taken,
    pool: FramePool<M>,
}

impl<M> fmt::Debug for JobStream<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobStream")
            .field("stream", &self.taken.stream().name())
            .field("buffers", &self.pool.buffers())
            .finish()
    }
}

impl<M> JobStream<M> {
    /// The job stream's name, as the device answered it.
    pub fn name(&self) -> &str {
        self.taken.stream().name()
    }

    /// Buffers in the stream.
    pub fn buffers(&self) -> usize {
        self.pool.buffers()
    }

    /// Completions the reader was too late for, in total (`FrameStream::skipped`). On a job stream
    /// every completion is read, so this stays 0 unless the reader falls behind.
    pub fn skipped(&self) -> u64 {
        self.taken.stream().skipped()
    }

    /// Bytes in each buffer, as the stream's open reply gives them.
    pub fn buffer_bytes(&self) -> usize {
        self.taken.stream().layout().buffer_bytes as usize
    }

    /// Bytes in each posted job, as the stream's open reply gives them.
    pub fn job_bytes(&self) -> usize {
        self.taken.stream().layout().job_bytes as usize
    }

    /// The pool over the stream's buffers. Take leases from it with [`FramePool::acquire_at`].
    pub fn pool(&self) -> &FramePool<M> {
        &self.pool
    }

    /// Posts one job. `Ok(false)` means the job queue is full and nothing was posted.
    pub fn post(&mut self, job: &[u8]) -> Result<bool, Error> {
        self.taken
            .stream_mut()
            .post_job(job)
            .map_err(Error::from_client)
    }

    /// Waits up to `timeout` for the next completion frame; `None` after the timeout.
    ///
    /// # Errors
    ///
    /// [`Error::StreamEnded`] once the host has ended the stream and no completion is left.
    pub fn wait(&mut self, timeout: Duration) -> Result<Option<HeldFrame>, Error> {
        let frame = self
            .taken
            .stream_mut()
            .next_frame(timeout)
            .map_err(Error::from_client)?;
        match frame {
            Some(frame) => Ok(Some(frame)),
            None => match self.taken.stream().end() {
                None => Ok(None),
                Some(end) => Err(Error::StreamEnded(end.into())),
            },
        }
    }

    /// The bytes of a frame this stream handed out by [`JobStream::wait`].
    pub fn bytes(&self, frame: &HeldFrame) -> &[u8] {
        self.taken.stream().bytes(frame)
    }

    /// Gives a completion frame back to Lemnos.
    pub fn release(&mut self, frame: HeldFrame) -> Result<(), Error> {
        self.taken
            .stream_mut()
            .release(frame)
            .map_err(Error::from_client)
    }

    /// Closes the stream on the daemon and returns the daemon's answer.
    pub fn close(self) -> Result<(), Error> {
        self.taken.close()
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use lemnos_wire::ring::{RingError, SharedLayout as WireLayout};

    #[test]
    fn a_shared_layout_refusal_keeps_its_structure_and_both_values() {
        for (wire, want) in [
            (WireLayout::Ring, SharedLayout::Ring),
            (WireLayout::Marks, SharedLayout::Marks),
        ] {
            let refused = lemnos_client::Error::Stream(RingError::Layout {
                which: wire,
                found: 1,
                expected: 2,
            });
            let error = Error::from_client(refused);
            let expected = LayoutRefusal {
                which: want,
                found: 1,
                expected: 2,
            };
            assert_eq!(error.layout_refusal(), Some(expected));
            let text = error.to_string();
            assert!(
                text.contains("layout 1") && text.contains("layout 2"),
                "{text}"
            );
            assert!(text.contains(want.what()), "{text}");
        }
    }

    #[test]
    fn a_contract_mismatch_at_bind_is_its_own_error_and_its_sentence_keeps_both_hashes() {
        let client = lemnos_client::Error::ContractMismatch {
            contract: "camera.sensor/1".to_string(),
            ours: 0x33b8_60b0_2987_233d,
            theirs: 0x4d6f_4b4b_ddf7_8636,
        };
        let error = Error::from_client(client);
        assert!(matches!(error, Error::ContractMismatch { .. }), "{error:?}");
        assert_eq!(error.contract_mismatch(), Some("camera.sensor/1"));
        assert_eq!(error.layout_refusal(), None);
        assert_eq!(
            error.to_string(),
            "this Styx and the board's Lemnos are different builds: contract camera.sensor/1: a base method is laid out differently (the device's base hash 0x4d6f4b4bddf78636, this client's 0x33b860b02987233d)"
        );
    }

    #[test]
    fn other_stream_errors_stay_client_errors() {
        let error = Error::from_client(lemnos_client::Error::Stream(RingError::BadBuffer));
        assert!(matches!(error, Error::Client(_)));
        assert_eq!(error.layout_refusal(), None);
    }
}
