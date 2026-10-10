//! Buffers and streaming: REQBUFS (MMAP, DMABUF), QUERYBUF, mmap, EXPBUF, QBUF/DQBUF,
//! STREAMON/OFF.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Duration;

use super::raw::{self, zeroed};
use super::{BufType, VideoDevice};
use crate::flags::flags;
use crate::ioctl;
use crate::{Error, Mapping, Result};

/// The most planes a buffer has (`VIDEO_MAX_PLANES`).
pub const MAX_PLANES: usize = raw::VIDEO_MAX_PLANES;

/// A buffer's planes, held inline: no allocation per queue or dequeue. Derefs to the planes
/// as a slice. A list longer than [`MAX_PLANES`] keeps the first ones and sets
/// [`Planes::is_over`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Planes<T> {
    len: usize,
    over: bool,
    items: [T; MAX_PLANES],
}

impl<T: Copy + Default> Planes<T> {
    /// No planes.
    pub fn new() -> Self {
        Self {
            len: 0,
            over: false,
            items: [T::default(); MAX_PLANES],
        }
    }

    /// One plane.
    pub fn one(p: T) -> Self {
        let mut out = Self::new();
        out.items[0] = p;
        out.len = 1;
        out
    }

    /// Whether more planes were given than fit (the buffer is then not queued).
    pub fn is_over(&self) -> bool {
        self.over
    }
}

impl<T: Copy + Default> Default for Planes<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Default> FromIterator<T> for Planes<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut out = Self::new();
        for p in iter {
            if out.len == MAX_PLANES {
                out.over = true;
                break;
            }
            out.items[out.len] = p;
            out.len += 1;
        }
        out
    }
}

impl<T> std::ops::Deref for Planes<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.items[..self.len]
    }
}

impl<T> std::ops::DerefMut for Planes<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.items[..self.len]
    }
}

impl<'a, T> IntoIterator for &'a Planes<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// How buffer memory is provided (`enum v4l2_memory`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Memory {
    /// Driver-allocated buffers, mapped with `mmap` (and exportable as dma-bufs).
    Mmap = 1,
    /// User pointers.
    UserPtr = 2,
    /// Imported dma-buf file descriptors.
    DmaBuf = 4,
}

impl Memory {
    fn from_raw(v: u32) -> Option<Self> {
        match v {
            1 => Some(Memory::Mmap),
            2 => Some(Memory::UserPtr),
            4 => Some(Memory::DmaBuf),
            _ => None,
        }
    }
}

flags! {
    /// What a queue supports (`V4L2_BUF_CAP_*`), reported by `VIDIOC_REQBUFS`.
    pub struct BufferCapabilities: u32 {
        const SUPPORTS_MMAP = 1 << 0;
        const SUPPORTS_USERPTR = 1 << 1;
        const SUPPORTS_DMABUF = 1 << 2;
        const SUPPORTS_REQUESTS = 1 << 3;
        const SUPPORTS_ORPHANED_BUFS = 1 << 4;
        const SUPPORTS_M2M_HOLD_CAPTURE_BUF = 1 << 5;
        const SUPPORTS_MMAP_CACHE_HINTS = 1 << 6;
        const SUPPORTS_MAX_NUM_BUFFERS = 1 << 7;
        const SUPPORTS_REMOVE_BUFS = 1 << 8;
    }
}

flags! {
    /// Buffer state and metadata flags (`V4L2_BUF_FLAG_*`).
    pub struct BufferFlags: u32 {
        const MAPPED = 0x0000_0001;
        const QUEUED = 0x0000_0002;
        const DONE = 0x0000_0004;
        const KEYFRAME = 0x0000_0008;
        const PFRAME = 0x0000_0010;
        const BFRAME = 0x0000_0020;
        const ERROR = 0x0000_0040;
        const IN_REQUEST = 0x0000_0080;
        const TIMECODE = 0x0000_0100;
        const M2M_HOLD_CAPTURE_BUF = 0x0000_0200;
        const PREPARED = 0x0000_0400;
        const NO_CACHE_INVALIDATE = 0x0000_0800;
        const NO_CACHE_CLEAN = 0x0000_1000;
        const TIMESTAMP_MONOTONIC = 0x0000_2000;
        const TIMESTAMP_COPY = 0x0000_4000;
        const TSTAMP_SRC_SOE = 0x0001_0000;
        const LAST = 0x0010_0000;
        const REQUEST_FD = 0x0080_0000;
    }
}

/// The result of `VIDIOC_REQBUFS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestedBuffers {
    /// Number of buffers allocated (may differ from the number asked for).
    pub count: u32,
    /// What the queue supports.
    pub capabilities: BufferCapabilities,
}

/// One plane of a buffer, as reported by `VIDIOC_QUERYBUF`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneInfo {
    /// Plane size in bytes.
    pub length: u32,
    /// The `mmap` offset (MMAP memory).
    pub mem_offset: u32,
    /// Bytes of payload.
    pub bytes_used: u32,
    /// Offset of the data from the start of the plane.
    pub data_offset: u32,
}

/// A buffer's description (`VIDIOC_QUERYBUF`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferInfo {
    /// Buffer index.
    pub index: u32,
    /// Queue type.
    pub buf_type: BufType,
    /// Memory type.
    pub memory: Option<Memory>,
    /// State flags.
    pub flags: BufferFlags,
    /// The planes (one for single-planar queues).
    pub planes: Vec<PlaneInfo>,
}

/// A plane to queue. For MMAP buffers only `bytes_used` matters (and only for output queues);
/// for DMABUF buffers `dmabuf` is the dma-buf to import.
#[derive(Clone, Copy, Debug, Default)]
pub struct QueuePlane<'fd> {
    /// The dma-buf to import (DMABUF memory).
    pub dmabuf: Option<BorrowedFd<'fd>>,
    /// Size of the dma-buf plane in bytes (DMABUF; 0 lets the driver use the dma-buf size).
    pub length: u32,
    /// Payload size (output queues).
    pub bytes_used: u32,
    /// Offset of the data in the plane (output queues, multi-planar).
    pub data_offset: u32,
}

/// A buffer to queue with [`VideoDevice::queue`].
#[derive(Clone, Debug)]
pub struct QueueBuffer<'fd> {
    /// Queue type.
    pub buf_type: BufType,
    /// Memory type the queue was set up with.
    pub memory: Memory,
    /// Buffer index.
    pub index: u32,
    /// Planes (one for single-planar queues; may be empty for capture MMAP buffers).
    pub planes: Planes<QueuePlane<'fd>>,
    /// Queue the buffer into this media request instead of directly.
    pub request: Option<BorrowedFd<'fd>>,
    /// Field order (output queues; 0 lets the driver choose).
    pub field: u32,
    /// Timestamp to attach (output queues; copied to the capture side by m2m devices).
    pub timestamp: Duration,
}

impl<'fd> QueueBuffer<'fd> {
    /// An MMAP buffer with no payload information (capture).
    pub fn mmap(buf_type: BufType, index: u32) -> Self {
        Self {
            buf_type,
            memory: Memory::Mmap,
            index,
            planes: Planes::new(),
            request: None,
            field: 0,
            timestamp: Duration::ZERO,
        }
    }

    /// A DMABUF buffer importing one dma-buf per plane.
    pub fn dmabuf(buf_type: BufType, index: u32, planes: &[BorrowedFd<'fd>]) -> Self {
        Self {
            planes: planes
                .iter()
                .map(|&fd| QueuePlane {
                    dmabuf: Some(fd),
                    ..Default::default()
                })
                .collect(),
            memory: Memory::DmaBuf,
            ..Self::mmap(buf_type, index)
        }
    }
}

/// A buffer returned by [`VideoDevice::dequeue`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DequeuedBuffer {
    /// Buffer index.
    pub index: u32,
    /// Frame sequence number from the driver.
    pub sequence: u32,
    /// Flags (`ERROR` marks a corrupted frame; `LAST` the end of a stream).
    pub flags: BufferFlags,
    /// Field order.
    pub field: u32,
    /// Capture timestamp; `CLOCK_MONOTONIC` when `flags` has `TIMESTAMP_MONOTONIC`.
    pub timestamp: Duration,
    /// Per-plane payload: `(bytes_used, data_offset)`.
    pub planes: Planes<(u32, u32)>,
}

impl DequeuedBuffer {
    /// Total payload bytes over all planes.
    pub fn bytes_used(&self) -> usize {
        self.planes.iter().map(|&(used, _)| used as usize).sum()
    }
}

fn timeval_to_duration(tv: libc::timeval) -> Duration {
    Duration::new(
        tv.tv_sec.max(0) as u64,
        (tv.tv_usec.max(0) as u32).saturating_mul(1000),
    )
}

fn duration_to_timeval(d: Duration) -> libc::timeval {
    libc::timeval {
        tv_sec: d.as_secs() as _,
        tv_usec: d.subsec_micros() as _,
    }
}

impl VideoDevice {
    /// Allocates (or, with `count == 0`, frees) the buffers of a queue (`VIDIOC_REQBUFS`).
    pub fn request_buffers(
        &self,
        buf_type: BufType,
        memory: Memory,
        count: u32,
    ) -> Result<RequestedBuffers> {
        let mut raw = raw::v4l2_requestbuffers {
            count,
            type_: buf_type.to_raw(),
            memory: memory as u32,
            ..Default::default()
        };
        // SAFETY: VIDIOC_REQBUFS takes a `v4l2_requestbuffers`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_REQBUFS, &mut raw)? };
        Ok(RequestedBuffers {
            count: raw.count,
            capabilities: BufferCapabilities(raw.capabilities),
        })
    }

    /// Frees all buffers of a queue (`VIDIOC_REQBUFS` with count 0). Mappings and exported
    /// dma-bufs keep the memory alive until they are dropped.
    pub fn free_buffers(&self, buf_type: BufType, memory: Memory) -> Result<()> {
        self.request_buffers(buf_type, memory, 0).map(|_| ())
    }

    /// Describes a buffer (`VIDIOC_QUERYBUF`).
    pub fn query_buffer(&self, buf_type: BufType, index: u32) -> Result<BufferInfo> {
        let mut planes: [raw::v4l2_plane; raw::VIDEO_MAX_PLANES] = zeroed();
        let mut buf: raw::v4l2_buffer = zeroed();
        buf.index = index;
        buf.type_ = buf_type.to_raw();
        if buf_type.is_multiplanar() {
            buf.m.planes = planes.as_mut_ptr();
            buf.length = raw::VIDEO_MAX_PLANES as u32;
        }
        // SAFETY: VIDIOC_QUERYBUF takes a `v4l2_buffer`; for multi-planar queues `m.planes`
        // points to `length` planes that outlive the call.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_QUERYBUF, &mut buf)? };
        let planes = if buf_type.is_multiplanar() {
            let n = (buf.length as usize).min(raw::VIDEO_MAX_PLANES);
            planes[..n]
                .iter()
                .map(|p| PlaneInfo {
                    length: p.length,
                    // SAFETY: plain integer union member; for MMAP queues the kernel set it.
                    mem_offset: unsafe { p.m.offset },
                    bytes_used: p.bytesused,
                    data_offset: p.data_offset,
                })
                .collect()
        } else {
            vec![PlaneInfo {
                length: buf.length,
                // SAFETY: plain integer union member; for MMAP queues the kernel set it.
                mem_offset: unsafe { buf.m.offset },
                bytes_used: buf.bytesused,
                data_offset: 0,
            }]
        };
        Ok(BufferInfo {
            index: buf.index,
            buf_type,
            memory: Memory::from_raw(buf.memory),
            flags: BufferFlags(buf.flags),
            planes,
        })
    }

    /// Maps every plane of an MMAP buffer into memory (`VIDIOC_QUERYBUF` + `mmap`).
    pub fn map_buffer(&self, buf_type: BufType, index: u32) -> Result<Vec<Mapping>> {
        let info = self.query_buffer(buf_type, index)?;
        let writable = buf_type.is_output();
        info.planes
            .iter()
            .map(|p| {
                Mapping::new(
                    self.as_fd(),
                    p.length as usize,
                    i64::from(p.mem_offset),
                    writable,
                )
            })
            .collect()
    }

    /// Exports one plane of an MMAP buffer as a dma-buf (`VIDIOC_EXPBUF`), close-on-exec and
    /// read-write.
    pub fn export_buffer(&self, buf_type: BufType, index: u32, plane: u32) -> Result<OwnedFd> {
        let mut raw = raw::v4l2_exportbuffer {
            type_: buf_type.to_raw(),
            index,
            plane,
            flags: (libc::O_CLOEXEC | libc::O_RDWR) as u32,
            ..Default::default()
        };
        // SAFETY: VIDIOC_EXPBUF takes a `v4l2_exportbuffer`.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_EXPBUF, &mut raw)? };
        if raw.fd < 0 {
            return Err(Error::Invalid("VIDIOC_EXPBUF returned no fd".into()));
        }
        // SAFETY: the kernel returned a new dma-buf descriptor that we now own.
        Ok(unsafe { OwnedFd::from_raw_fd(raw.fd) })
    }

    /// Queues a buffer (`VIDIOC_QBUF`).
    pub fn queue(&self, req: &QueueBuffer<'_>) -> Result<()> {
        let mut planes: [raw::v4l2_plane; raw::VIDEO_MAX_PLANES] = zeroed();
        if req.planes.is_over() {
            return Err(Error::Invalid(format!("more than {MAX_PLANES} planes")));
        }
        let mut buf: raw::v4l2_buffer = zeroed();
        buf.index = req.index;
        buf.type_ = req.buf_type.to_raw();
        buf.memory = req.memory as u32;
        buf.field = req.field;
        buf.timestamp = duration_to_timeval(req.timestamp);
        if let Some(request) = req.request {
            buf.flags |= BufferFlags::REQUEST_FD.0;
            buf.request_fd = request.as_raw_fd();
        }
        if req.buf_type.is_multiplanar() {
            for (dst, src) in planes.iter_mut().zip(req.planes.iter()) {
                dst.bytesused = src.bytes_used;
                dst.length = src.length;
                dst.data_offset = src.data_offset;
                if let Some(fd) = src.dmabuf {
                    dst.m.fd = fd.as_raw_fd();
                }
            }
            buf.m.planes = planes.as_mut_ptr();
            buf.length = req.planes.len() as u32;
        } else if let Some(p) = req.planes.first() {
            buf.bytesused = p.bytes_used;
            buf.length = p.length;
            if let Some(fd) = p.dmabuf {
                buf.m.fd = fd.as_raw_fd();
            }
        }
        if req.memory == Memory::DmaBuf && req.planes.iter().any(|p| p.dmabuf.is_none()) {
            return Err(Error::Invalid(
                "DMABUF queue needs a dma-buf for every plane".into(),
            ));
        }
        // SAFETY: VIDIOC_QBUF takes a `v4l2_buffer`; `m.planes` (multi-planar) points to
        // `length` planes on this stack frame. Borrowed dma-buf fds stay open for the call.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_QBUF, &mut buf)? };
        Ok(())
    }

    /// Dequeues a finished buffer (`VIDIOC_DQBUF`). Returns `Ok(None)` when none is ready
    /// (the device is non-blocking by default).
    pub fn dequeue(&self, buf_type: BufType, memory: Memory) -> Result<Option<DequeuedBuffer>> {
        let mut planes: [raw::v4l2_plane; raw::VIDEO_MAX_PLANES] = zeroed();
        let mut buf: raw::v4l2_buffer = zeroed();
        buf.type_ = buf_type.to_raw();
        buf.memory = memory as u32;
        if buf_type.is_multiplanar() {
            buf.m.planes = planes.as_mut_ptr();
            buf.length = raw::VIDEO_MAX_PLANES as u32;
        }
        // SAFETY: VIDIOC_DQBUF takes a `v4l2_buffer`; `m.planes` (multi-planar) points to
        // `length` planes on this stack frame.
        match unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_DQBUF, &mut buf) } {
            Ok(_) => {}
            Err(e) if e.is_would_block() => return Ok(None),
            Err(e) => return Err(e),
        }
        let planes = if buf_type.is_multiplanar() {
            let n = (buf.length as usize).min(raw::VIDEO_MAX_PLANES);
            planes[..n]
                .iter()
                .map(|p| (p.bytesused, p.data_offset))
                .collect()
        } else {
            Planes::one((buf.bytesused, 0))
        };
        Ok(Some(DequeuedBuffer {
            index: buf.index,
            sequence: buf.sequence,
            flags: BufferFlags(buf.flags),
            field: buf.field,
            timestamp: timeval_to_duration(buf.timestamp),
            planes,
        }))
    }

    /// Starts streaming on a queue (`VIDIOC_STREAMON`).
    pub fn stream_on(&self, buf_type: BufType) -> Result<()> {
        let mut ty = buf_type.to_raw() as libc::c_int;
        // SAFETY: VIDIOC_STREAMON takes a pointer to an int buffer type.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_STREAMON, &mut ty)? };
        Ok(())
    }

    /// Stops streaming on a queue (`VIDIOC_STREAMOFF`); all buffers return to userspace.
    pub fn stream_off(&self, buf_type: BufType) -> Result<()> {
        let mut ty = buf_type.to_raw() as libc::c_int;
        // SAFETY: VIDIOC_STREAMOFF takes a pointer to an int buffer type.
        unsafe { ioctl::ioctl(self.as_fd(), raw::VIDIOC_STREAMOFF, &mut ty)? };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_convert() {
        let tv = libc::timeval {
            tv_sec: 12,
            tv_usec: 345_678,
        };
        let d = timeval_to_duration(tv);
        assert_eq!(d, Duration::from_micros(12_345_678));
        let back = duration_to_timeval(d);
        assert_eq!((back.tv_sec, back.tv_usec), (12, 345_678));
    }

    #[test]
    fn buffer_flags_debug() {
        let f = BufferFlags::DONE | BufferFlags::TIMESTAMP_MONOTONIC;
        assert_eq!(format!("{f:?}"), "DONE | TIMESTAMP_MONOTONIC");
    }
}
