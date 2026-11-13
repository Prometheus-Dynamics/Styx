//! Styx's client for Lemnos cameras, over the `lemnos-client` socket.
//!
//! [`find_chain`] finds the `camera.sensor/1` device by tag and follows `link` to its receiver and,
//! when it has one, its ISP. [`Frames`] opens the receiver's frame stream and turns each of its
//! buffers into [`styx_frame`] buffers, once, so that every frame is a [`FrameLease`] over the
//! memory Lemnos filled: no copy. Lemnos carries the frame identity and timestamp as opaque values;
//! both reach the lease unchanged.
//!
//! Lemnos does no 3A and holds no tuning. This crate is discovery and buffers only.

use std::fmt;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use lemnos_client::{Client, DeviceEntry, FrameStream, HeldFrame, StreamPlatform, follow};
use lemnos_linux_stream::LinuxStreams;
use lemnos_wire::ring::{BUFFER_ALIGN, ByteMemory, WordMemory};
use styx_format::{FourCc, MediaFormat};
use styx_frame::{
    AcquireError, BufferMemory, BufferSize, CpuAccess, FdMemory, FrameEntry, FrameLease, FramePool,
    Layouts, MemoryKind, PoolError,
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
    Client::connect(path, name).map_err(Error::Client)
}

/// A camera's devices, found by tag and link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub sensor: DeviceEntry,
    pub receiver: DeviceEntry,
    pub isp: Option<DeviceEntry>,
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
    /// The sensor's link does not name a device with `camera.receiver/1`.
    NotReceiver { device: String, tags: Vec<String> },
    /// The receiver's link names a device that is not `camera.isp/1`.
    NotIsp { device: String, tags: Vec<String> },
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
        }
    }
}

impl std::error::Error for ChainError {}

fn has_tag(device: &DeviceEntry, tag: &str) -> bool {
    device.tags.iter().any(|t| t == tag)
}

/// Finds the one sensor in `devices`, then its receiver and ISP by following `link`.
///
/// A sensor with no receiver is refused. A receiver with no link is a camera without an ISP,
/// which is valid.
pub fn find_chain(devices: &[DeviceEntry]) -> Result<Chain, ChainError> {
    let sensors: Vec<&DeviceEntry> = devices.iter().filter(|d| has_tag(d, SENSOR_TAG)).collect();
    let sensor = match sensors.as_slice() {
        [] => return Err(ChainError::NoSensor),
        [one] => (*one).clone(),
        many => return Err(ChainError::SensorCount(many.len())),
    };
    let path = follow(devices, &sensor.name);
    let receiver = match path.get(1) {
        Some(device) => (*device).clone(),
        None => return Err(ChainError::NotLinked(sensor.name.clone())),
    };
    if !has_tag(&receiver, RECEIVER_TAG) {
        return Err(ChainError::NotReceiver {
            device: receiver.name.clone(),
            tags: receiver.tags.clone(),
        });
    }
    let path = follow(devices, &receiver.name);
    let isp = match path.get(1) {
        Some(device) if has_tag(device, ISP_TAG) => Some((*device).clone()),
        Some(device) => {
            return Err(ChainError::NotIsp {
                device: device.name.clone(),
                tags: device.tags.clone(),
            });
        }
        None => None,
    };
    Ok(Chain {
        sensor,
        receiver,
        isp,
    })
}

/// Errors from opening and reading frame streams.
#[derive(Debug)]
pub enum Error {
    Client(lemnos_client::Error),
    Chain(ChainError),
    /// The stream's buffers could not be mapped.
    Map(io::Error),
    /// The stream has no buffers this crate can map (neither one memfd nor one dma-buf each).
    NoBuffers,
    /// A frame names a buffer outside the stream.
    BadBuffer(u32),
    /// A frame named a buffer whose lease is still out; the frame was given back to Lemnos.
    Held(u32),
    Pool(PoolError),
    Acquire(AcquireError),
    Layout(styx_format::LayoutError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(e) => write!(f, "lemnos client: {e}"),
            Self::Chain(e) => write!(f, "{e}"),
            Self::Map(e) => write!(f, "mapping stream buffers: {e}"),
            Self::NoBuffers => write!(f, "stream has no mappable buffers"),
            Self::BadBuffer(b) => write!(f, "frame names buffer {b}, which is out of range"),
            Self::Held(b) => write!(f, "buffer {b} still held; frame dropped"),
            Self::Pool(e) => write!(f, "frame pool: {e:?}"),
            Self::Acquire(e) => write!(f, "frame acquire: {e:?}"),
            Self::Layout(e) => write!(f, "frame layout: {e:?}"),
        }
    }
}

impl std::error::Error for Error {}

/// The stream's buffer memory as mapped from its descriptors.
#[derive(Debug, Default)]
struct Mapped {
    /// One memfd for every buffer, at a stride of `buffer_bytes` rounded to `BUFFER_ALIGN`.
    whole: Option<Arc<FdMemory>>,
    /// One dma-buf per buffer.
    each: Vec<Arc<FdMemory>>,
    /// The buffer count and size from the ring's header words.
    buffers: u32,
    buffer_bytes: u32,
}

/// Wraps [`LinuxStreams`] so the buffer descriptors the client maps are also mapped here, as
/// [`FdMemory`]. Both maps view the same memory; nothing is copied.
#[derive(Debug)]
struct MappingPlatform {
    inner: LinuxStreams,
    mapped: Mutex<Mapped>,
}

impl StreamPlatform for MappingPlatform {
    fn recv_with_descriptors(
        &self,
        socket: BorrowedFd<'_>,
        buf: &mut [u8],
        fds: &mut Vec<OwnedFd>,
    ) -> io::Result<usize> {
        self.inner.recv_with_descriptors(socket, buf, fds)
    }

    fn map_ring(&self, fd: BorrowedFd<'_>, bytes: usize) -> io::Result<Arc<dyn WordMemory>> {
        let ring = self.inner.map_ring(fd, bytes)?;
        {
            let words = ring.words();
            let mut mapped = self.lock();
            mapped.buffers =
                words[ring_word::BUFFERS].load(std::sync::atomic::Ordering::Acquire) as u32;
            mapped.buffer_bytes =
                words[ring_word::BUFFER_BYTES].load(std::sync::atomic::Ordering::Acquire) as u32;
        }
        Ok(ring)
    }

    fn map_buffers(
        &self,
        fd: BorrowedFd<'_>,
        bytes: usize,
        writable: bool,
    ) -> io::Result<Box<dyn ByteMemory>> {
        let mine = FdMemory::memfd(fd.try_clone_to_owned()?, bytes, writable)?;
        self.lock().whole = Some(Arc::new(mine));
        self.inner.map_buffers(fd, bytes, writable)
    }

    fn map_buffer(
        &self,
        fd: BorrowedFd<'_>,
        bytes: usize,
        writable: bool,
    ) -> io::Result<Box<dyn ByteMemory>> {
        let mine = FdMemory::dmabuf(fd.try_clone_to_owned()?, bytes, writable)?;
        self.lock().each.push(Arc::new(mine));
        self.inner.map_buffer(fd, bytes, writable)
    }

    fn wait_doorbell(&self, fd: BorrowedFd<'_>, timeout: Duration) -> io::Result<bool> {
        self.inner.wait_doorbell(fd, timeout)
    }
}

impl MappingPlatform {
    fn lock(&self) -> std::sync::MutexGuard<'_, Mapped> {
        self.mapped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Ring header word indices, as `lemnos-wire` lays them out.
mod ring_word {
    pub use lemnos_wire::ring::offsets::{BUFFER_BYTES, BUFFERS};
}

/// One buffer's bytes inside a mapping: the pool's view of it. Holds the mapping alive.
struct Slice {
    base: Arc<FdMemory>,
    offset: usize,
    len: usize,
}

impl fmt::Debug for Slice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slice")
            .field("offset", &self.offset)
            .field("len", &self.len)
            .finish()
    }
}

// SAFETY: `cpu_ptr` is `base`'s start plus `offset`, with `offset + len` inside `base.len()`
// (checked when the slice is made). `base` is held for the slice's life, so the mapping does not
// go away, and the slice reads and writes only the bytes of the one buffer it names.
unsafe impl BufferMemory for Slice {
    fn kind(&self) -> MemoryKind {
        self.base.kind()
    }

    fn len(&self) -> usize {
        self.len
    }

    fn cpu_ptr(&self) -> Option<NonNull<u8>> {
        let base = self.base.cpu_ptr()?;
        // SAFETY: `offset` is inside the mapping (checked in `slice`), so the offset pointer stays
        // in the same allocation.
        let ptr = unsafe { base.as_ptr().add(self.offset) };
        NonNull::new(ptr)
    }

    fn cpu_writable(&self) -> bool {
        self.base.cpu_writable()
    }

    fn begin_cpu(&self, access: CpuAccess) {
        self.base.begin_cpu(access);
    }

    fn end_cpu(&self, access: CpuAccess) {
        self.base.end_cpu(access);
    }

    fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.base.fd()
    }
}

fn slice(base: &Arc<FdMemory>, offset: usize, len: usize) -> Result<Slice, Error> {
    let end = offset.checked_add(len).ok_or(Error::NoBuffers)?;
    if end > base.len() {
        return Err(Error::NoBuffers);
    }
    Ok(Slice {
        base: Arc::clone(base),
        offset,
        len,
    })
}

/// Opens `stream` on `device` and maps its buffers, as one pool. Frames are `code` at `width` x
/// `height`.
pub fn open_frames(
    client: &Client,
    device: u32,
    stream: &str,
    code: FourCc,
    width: u32,
    height: u32,
) -> Result<Frames, Error> {
    let platform = Arc::new(MappingPlatform {
        inner: LinuxStreams,
        mapped: Mutex::new(Mapped::default()),
    });
    let opened = client
        .open_stream(device, stream, platform.clone())
        .map_err(Error::Client)?;
    let mapped = std::mem::take(&mut *platform.lock());
    let format = MediaFormat::with_defaults(
        code,
        styx_format::Resolution::new(width, height).ok_or(Error::NoBuffers)?,
    );
    let layouts =
        styx_frame::packed_layouts(code, width, height, None, None).map_err(Error::Layout)?;
    let buffer_bytes = mapped.buffer_bytes as usize;
    let memories = match &mapped.whole {
        Some(whole) => whole_memories(whole, mapped.buffers as usize, buffer_bytes)?,
        None => each_memories(&mapped.each, buffer_bytes)?,
    };
    if memories.is_empty() {
        return Err(Error::NoBuffers);
    }
    let pool = pool_over(memories, buffer_bytes)?;
    let count = pool.buffers();
    Ok(Frames {
        stream: opened,
        pool,
        pending: (0..count).map(|_| None).collect(),
        format,
        layouts,
        held_drops: 0,
    })
}

/// One memory per buffer, cut from a memfd holding every buffer at the ring's stride.
pub fn whole_memories(
    whole: &Arc<FdMemory>,
    buffers: usize,
    buffer_bytes: usize,
) -> Result<Vec<Box<dyn BufferMemory>>, Error> {
    let stride = buffer_bytes.next_multiple_of(BUFFER_ALIGN as usize);
    (0..buffers)
        .map(|i| {
            let offset = i.checked_mul(stride).ok_or(Error::NoBuffers)?;
            slice(whole, offset, buffer_bytes).map(|s| Box::new(s) as Box<dyn BufferMemory>)
        })
        .collect()
}

/// One memory per buffer, each a dma-buf from its first byte.
pub fn each_memories(
    each: &[Arc<FdMemory>],
    buffer_bytes: usize,
) -> Result<Vec<Box<dyn BufferMemory>>, Error> {
    each.iter()
        .map(|one| slice(one, 0, buffer_bytes).map(|s| Box::new(s) as Box<dyn BufferMemory>))
        .collect()
}

/// One pool over all of a stream's buffers. Frames are acquired with
/// [`FramePool::acquire_at`], at the buffer index the ring names.
pub fn pool_over(
    memories: Vec<Box<dyn BufferMemory>>,
    buffer_bytes: usize,
) -> Result<FramePool<usize>, Error> {
    let size = BufferSize {
        payload: buffer_bytes,
        gpu_slack: 0,
    };
    FramePool::new(size, memories).map_err(Error::Pool)
}

/// A frame stream's buffers as one frame pool, and the stream it came from.
///
/// A buffer goes back to Lemnos only when no lease on it is out: [`Frames::next`] first gives back
/// every buffer whose lease has dropped, then waits for a frame. A frame that names a buffer whose
/// lease is still out is counted in [`Frames::held_drops`] and given back at once, never waited on.
pub struct Frames {
    stream: FrameStream,
    pool: FramePool<usize>,
    /// The Lemnos frame of each buffer, until its lease has dropped.
    pending: Vec<Option<HeldFrame>>,
    format: MediaFormat,
    layouts: Layouts,
    held_drops: u64,
}

impl fmt::Debug for Frames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frames")
            .field("stream", &self.stream.name())
            .field("buffers", &self.pool.buffers())
            .field("held_drops", &self.held_drops)
            .finish()
    }
}

impl Frames {
    /// Buffers in the stream.
    pub fn buffers(&self) -> usize {
        self.pool.buffers()
    }

    /// Frames dropped because their buffer's previous lease was still out.
    pub fn held_drops(&self) -> u64 {
        self.held_drops
    }

    /// Gives back to Lemnos every buffer whose lease has dropped. A buffer is free when a probe
    /// lease can be taken on it; the probe is dropped at once.
    fn sweep(&mut self) -> Result<(), Error> {
        for index in 0..self.pending.len() {
            if self.pending[index].is_none() {
                continue;
            }
            let probe = FrameEntry::new(0, 0, self.format, self.layouts);
            if let Ok(lease) = self.pool.acquire_at(index, probe) {
                drop(lease);
                if let Some(frame) = self.pending[index].take() {
                    self.stream.release(frame).map_err(Error::Client)?;
                }
            }
        }
        Ok(())
    }

    /// Waits up to `timeout` for a frame and returns it as a lease over its buffer.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<FrameLease<usize>>, Error> {
        self.sweep()?;
        let Some(frame) = self.stream.next_frame(timeout).map_err(Error::Client)? else {
            return Ok(None);
        };
        self.lease_for(frame).map(Some)
    }

    /// Takes a frame if one is ready, without waiting.
    pub fn try_next(&mut self) -> Result<Option<FrameLease<usize>>, Error> {
        self.sweep()?;
        let Some(frame) = self.stream.try_next().map_err(Error::Client)? else {
            return Ok(None);
        };
        self.lease_for(frame).map(Some)
    }

    fn lease_for(&mut self, frame: HeldFrame) -> Result<FrameLease<usize>, Error> {
        let index = frame.buffer() as usize;
        let entry = FrameEntry::new(
            frame.identity(),
            frame.timestamp_ns(),
            self.format,
            self.layouts,
        );
        match self.pool.acquire_at(index, entry) {
            Ok(mut lease) => {
                if let Some(meta) = lease.meta_mut() {
                    *meta = index;
                }
                self.pending[index] = Some(frame);
                Ok(lease)
            }
            Err(AcquireError::Held { .. }) => {
                self.held_drops += 1;
                self.stream.release(frame).map_err(Error::Client)?;
                Err(Error::Held(index as u32))
            }
            Err(e) => {
                self.stream.release(frame).map_err(Error::Client)?;
                Err(Error::Acquire(e))
            }
        }
    }

    /// Ends a lease. The buffer goes back to Lemnos on a later call, once the lease has dropped.
    pub fn release(&mut self, lease: FrameLease<usize>) {
        drop(lease);
    }

    /// Closes the stream; its frames are given back to the daemon.
    pub fn into_stream(self) -> FrameStream {
        self.stream
    }
}
