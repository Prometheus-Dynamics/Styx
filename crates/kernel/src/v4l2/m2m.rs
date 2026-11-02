//! A memory-to-memory device session ([`M2mSession`]): a scaler or converter (vim2m, the
//! Rockchip RGA, the K230's 2D unit) or a stateful encoder (vicodec, the K230's VPU). Frames
//! are copied into the device's source (`OUTPUT`) buffers, or handed to it by dma-buf
//! ([`M2mSession::open_importing`]), and come back in its destination (`CAPTURE`) buffers,
//! which the caller reads in place and gives back.

use std::collections::VecDeque;
use std::time::Duration;

use super::raw::{self, v4l2_encoder_cmd};
use super::{BufType, BufferFlags, DequeuedBuffer, Format, Memory, MmapQueue, VideoDevice};
use crate::{Error, Result, ioctl};
use std::os::fd::{AsFd, BorrowedFd};

/// `V4L2_ENC_CMD_STOP`.
const ENC_CMD_STOP: u32 = 1;

/// A destination buffer the device filled: read it with [`M2mSession::data`], then give it
/// back with [`M2mSession::release`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct M2mFrame {
    /// The buffer, with each plane's bytes used, its flags (`KEYFRAME`, `LAST`, `ERROR`) and
    /// the timestamp copied from its source frame.
    pub buffer: DequeuedBuffer,
}

impl M2mFrame {
    /// Whether this is the last buffer of a drained stream.
    pub fn is_last(&self) -> bool {
        self.buffer.flags.contains(BufferFlags::LAST)
    }

    /// Whether the device flagged the buffer as bad.
    pub fn is_error(&self) -> bool {
        self.buffer.flags.contains(BufferFlags::ERROR)
    }

    /// Whether an encoder made this a keyframe.
    pub fn is_keyframe(&self) -> bool {
        self.buffer.flags.contains(BufferFlags::KEYFRAME)
    }

    /// Whether an encoder flagged this a predicted frame (P or B). Encoders that flag nothing
    /// (vicodec) leave every frame not delta.
    pub fn is_delta(&self) -> bool {
        self.buffer
            .flags
            .intersects(BufferFlags::PFRAME | BufferFlags::BFRAME)
    }
}

/// One colour plane of an image: `rows` rows of `row_bytes` bytes, each `stride` bytes after
/// the one before.
#[derive(Clone, Copy, Debug)]
pub struct ImageRows<'a> {
    /// The rows; the last needs only `row_bytes` bytes.
    pub data: &'a [u8],
    /// Bytes from one row's start to the next.
    pub stride: usize,
    /// Bytes of picture in a row.
    pub row_bytes: usize,
    /// Number of rows.
    pub rows: usize,
}

/// Where one colour plane lies in a device buffer: its memory plane, its offset there and its
/// stride.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// The memory plane.
    pub buffer: usize,
    /// Bytes from the memory plane's start.
    pub offset: usize,
    /// Bytes from one row's start to the next.
    pub stride: usize,
}

/// One memory plane of a source frame handed over by dma-buf
/// ([`M2mSession::push_dmabuf`]).
#[derive(Clone, Copy, Debug)]
pub struct DmabufSource<'fd> {
    /// The dma-buf.
    pub fd: BorrowedFd<'fd>,
    /// Where the plane starts in it (0 on a single-planar queue, which has no offset).
    pub offset: u32,
    /// Bytes of picture from `offset`.
    pub bytes: u32,
}

/// The device's layout of an image's colour planes, each `(row_bytes, rows)`. Colour planes
/// map one-to-one to memory planes while there are memory planes; the rest follow each other
/// in the last memory plane (NV12 or YU12 in one buffer), their strides scaled from its
/// stride by their row widths, as V4L2 lays them out.
fn spans(format: &Format, shape: &[(usize, usize)]) -> Vec<Span> {
    let strides: Vec<usize> = match format {
        Format::Single(p) => vec![p.bytes_per_line as usize],
        Format::Multi(m) => m.planes.iter().map(|p| p.bytes_per_line as usize).collect(),
        Format::Meta(_) => Vec::new(),
    };
    let memory = strides.len().max(1);
    let mut out = Vec::with_capacity(shape.len());
    // The last memory plane's stride and row width, and the next free offset in it.
    let (mut base, mut offset) = ((0usize, 0usize), 0usize);
    for (i, &(row_bytes, rows)) in shape.iter().enumerate() {
        let span = if i < memory {
            let stride = strides.get(i).copied().unwrap_or(0).max(row_bytes);
            base = (stride, row_bytes);
            offset = 0;
            Span {
                buffer: i,
                offset: 0,
                stride,
            }
        } else {
            let stride = match base {
                (_, 0) => row_bytes,
                (stride, width) => (stride * row_bytes / width).max(row_bytes),
            };
            Span {
                buffer: memory - 1,
                offset,
                stride,
            }
        };
        offset += span.stride * rows;
        out.push(span);
    }
    out
}

/// A source slot: what the caller keeps alive while it is queued, and the dma-buf it last
/// imported.
struct Slot<H> {
    hold: Option<H>,
    last: Option<(u64, u64)>,
}

/// One stream through a memory-to-memory device: MMAP destination buffers, and MMAP source
/// buffers ([`Self::open`]) or source slots importing the caller's dma-bufs
/// ([`Self::open_importing`]). `H` is what the caller keeps alive while the device reads a
/// dma-buf (the frame over it), dropped when the device gives the slot back.
pub struct M2mSession<H = ()> {
    source: MmapQueue,
    dest: MmapQueue,
    /// Source buffers not queued.
    free: VecDeque<u32>,
    /// Per source slot: what the caller keeps alive while it is queued, and the dma-buf it
    /// last imported (device and inode), so a dma-buf goes back to the slot that has it
    /// attached already.
    held: Vec<Slot<H>>,
    source_format: Format,
    dest_format: Format,
}

impl M2mSession {
    /// Opens a stream on `dev` from frames of `source` to `dest`, with `buffers` buffers on
    /// each side (the driver may give more or fewer, at least one). The destination format is
    /// set first, as stateful encoders want; each side's queue is multi-planar when its format
    /// is. Every destination buffer is queued and both queues stream.
    pub fn open(dev: VideoDevice, source: &Format, dest: &Format, buffers: u32) -> Result<Self> {
        Self::open_with(dev, source, dest, buffers, false)
    }
}

impl<H> M2mSession<H> {
    /// [`Self::open`] with source slots that import the caller's dma-bufs
    /// ([`Self::push_dmabuf`]) instead of source buffers to copy into: no copy of the frame on
    /// its way in. Set the source format's stride to the frames' so the device reads them at
    /// their layout ([`Self::source_spans`] says where it expects each colour plane).
    pub fn open_importing(
        dev: VideoDevice,
        source: &Format,
        dest: &Format,
        buffers: u32,
    ) -> Result<Self> {
        Self::open_with(dev, source, dest, buffers, true)
    }

    /// [`Self::open`] (`import` false) or [`Self::open_importing`] (`import` true), for any
    /// `H`.
    pub fn open_with(
        dev: VideoDevice,
        source: &Format,
        dest: &Format,
        buffers: u32,
        import: bool,
    ) -> Result<Self> {
        let (source_type, dest_type) = (buf_type(source, true)?, buf_type(dest, false)?);
        let dest_format = dev.set_format(dest_type, dest)?;
        let source_format = dev.set_format(source_type, source)?;
        // An encoder sizes its coded buffers from the source format: read it back.
        let dest_format = dev.format(dest_type).unwrap_or(dest_format);
        let dest_dev = dev.try_clone()?;
        let source = if import {
            MmapQueue::importing(dev, source_type, buffers.max(1), "m2m source")?
        } else {
            MmapQueue::new(dev, source_type, buffers.max(1), "m2m source")?
        };
        let dest = MmapQueue::new(dest_dev, dest_type, buffers.max(1), "m2m destination")?;
        for index in 0..dest.len() as u32 {
            dest.queue(index, &[])?;
        }
        let free = (0..source.len() as u32).collect();
        let held = (0..source.len())
            .map(|_| Slot {
                hold: None,
                last: None,
            })
            .collect();
        source.stream_on()?;
        dest.stream_on()?;
        Ok(Self {
            source,
            dest,
            free,
            held,
            source_format,
            dest_format,
        })
    }

    /// Whether the source side imports dma-bufs ([`Self::open_importing`]).
    pub fn imports(&self) -> bool {
        self.source.memory == Memory::DmaBuf
    }

    /// Where the device expects each colour plane of a source frame, each `(row_bytes, rows)`
    /// in `shape`, at the source format's layout (see [`Self::push_image`]).
    pub fn source_spans(&self, shape: &[(usize, usize)]) -> Vec<Span> {
        spans(&self.source_format, shape)
    }

    /// Queues a source frame by dma-buf, one [`DmabufSource`] per memory plane of the source
    /// format, with `timestamp`; the device reads it in place. `hold` (the frame over the
    /// dma-bufs) is kept until the device gives the slot back. A free slot that last imported
    /// the same dma-buf is taken first, so the device does not attach and map it again.
    /// `Ok(false)` when every slot is still queued.
    pub fn push_dmabuf(
        &mut self,
        planes: &[DmabufSource<'_>],
        timestamp: Duration,
        hold: H,
    ) -> Result<bool> {
        if !self.imports() {
            return Err(Error::Invalid(
                "m2m source: push_dmabuf on a session opened without importing".into(),
            ));
        }
        let multi = self.source.buf_type.is_multiplanar();
        if planes.is_empty() || planes.len() > super::MAX_PLANES || (!multi && planes.len() != 1) {
            return Err(Error::Invalid(format!(
                "m2m source: {} dma-buf planes",
                planes.len()
            )));
        }
        if !multi && planes[0].offset != 0 {
            return Err(Error::Invalid(
                "m2m source: a single-planar queue reads from the dma-buf's start".into(),
            ));
        }
        self.reap()?;
        let key = crate::dma_heap::dmabuf_id(planes[0].fd);
        let at = self
            .free
            .iter()
            .position(|&i| key.is_some() && self.held[i as usize].last == key)
            .unwrap_or(0);
        let Some(index) = self.free.remove(at) else {
            return Ok(false);
        };
        let mut fds = [planes[0].fd; super::MAX_PLANES];
        for (fd, p) in fds.iter_mut().zip(planes) {
            *fd = p.fd;
        }
        let mut q = super::QueueBuffer::dmabuf(self.source.buf_type, index, &fds[..planes.len()]);
        q.timestamp = timestamp;
        for (qp, p) in q.planes.iter_mut().zip(planes) {
            // `bytesused` counts from the plane's start, `data_offset` included.
            qp.bytes_used = p.offset.saturating_add(p.bytes);
            qp.data_offset = p.offset;
        }
        if let Err(e) = self.source.dev.queue(&q) {
            self.free.push_front(index);
            return Err(e);
        }
        self.held[index as usize] = Slot {
            hold: Some(hold),
            last: key,
        };
        Ok(true)
    }

    /// The source format the driver took.
    pub fn source_format(&self) -> &Format {
        &self.source_format
    }

    /// The destination format the driver took.
    pub fn dest_format(&self) -> &Format {
        &self.dest_format
    }

    /// The device node.
    pub fn device(&self) -> &VideoDevice {
        &self.source.dev
    }

    /// Whether a source buffer is free for [`Self::push`] (after taking back the ones the
    /// device has finished reading).
    pub fn can_push(&mut self) -> Result<bool> {
        self.reap()?;
        Ok(!self.free.is_empty())
    }

    /// Copies one frame's planes into a free source buffer and queues it with `timestamp`
    /// (the device copies it to the destination buffer). `Ok(false)` when every source buffer
    /// is still queued; a plane larger than its buffer is an error.
    pub fn push(&mut self, planes: &[&[u8]], timestamp: Duration) -> Result<bool> {
        self.copies()?;
        self.reap()?;
        let Some(index) = self.free.pop_front() else {
            return Ok(false);
        };
        let maps = &mut self.source.maps[index as usize];
        if planes.len() != maps.len() {
            self.free.push_front(index);
            return Err(Error::Invalid(format!(
                "m2m source: {} planes for a {}-plane buffer",
                planes.len(),
                maps.len()
            )));
        }
        let mut used = [0u32; super::MAX_PLANES];
        for (p, (plane, map)) in planes.iter().zip(maps.iter_mut()).enumerate() {
            let dst = map.as_mut_slice();
            if plane.len() > dst.len() {
                self.free.push_front(index);
                return Err(Error::Invalid(format!(
                    "m2m source: plane {p} of {} bytes in a {}-byte buffer",
                    plane.len(),
                    dst.len()
                )));
            }
            dst[..plane.len()].copy_from_slice(plane);
            used[p] = plane.len() as u32;
        }
        let mut q = super::QueueBuffer::mmap(self.source.buf_type, index);
        q.timestamp = timestamp;
        q.planes = used[..planes.len()]
            .iter()
            .map(|&bytes_used| super::QueuePlane {
                bytes_used,
                ..Default::default()
            })
            .collect();
        if let Err(e) = self.source.dev.queue(&q) {
            self.free.push_front(index);
            return Err(e);
        }
        Ok(true)
    }

    /// Copies one picture into a free source buffer row by row, at the device's strides, and
    /// queues it with `timestamp`. The planes are the picture's colour planes in their usual
    /// order (Y then UV for NV12); the device may keep them in one buffer or one each.
    /// `Ok(false)` when every source buffer is still queued; a picture that does not fit is an
    /// error.
    pub fn push_image(&mut self, planes: &[ImageRows<'_>], timestamp: Duration) -> Result<bool> {
        self.copies()?;
        self.reap()?;
        let Some(index) = self.free.pop_front() else {
            return Ok(false);
        };
        let shape: Vec<_> = planes.iter().map(|p| (p.row_bytes, p.rows)).collect();
        let layout = spans(&self.source_format, &shape);
        let maps = &mut self.source.maps[index as usize];
        let mut used = [0u32; super::MAX_PLANES];
        for (p, (plane, span)) in planes.iter().zip(&layout).enumerate() {
            let Some(map) = maps.get_mut(span.buffer) else {
                self.free.push_front(index);
                return Err(Error::Invalid(format!(
                    "m2m source: plane {p} has no buffer plane"
                )));
            };
            let dst = map.as_mut_slice();
            let end = span.offset + span.stride * plane.rows.saturating_sub(1) + plane.row_bytes;
            let src_end = plane.stride * plane.rows.saturating_sub(1) + plane.row_bytes;
            if plane.rows > 0 && (end > dst.len() || src_end > plane.data.len()) {
                self.free.push_front(index);
                return Err(Error::Invalid(format!(
                    "m2m source: plane {p} ({} rows of {} bytes) does not fit",
                    plane.rows, plane.row_bytes
                )));
            }
            for r in 0..plane.rows {
                let d = span.offset + r * span.stride;
                let s = r * plane.stride;
                dst[d..d + plane.row_bytes].copy_from_slice(&plane.data[s..s + plane.row_bytes]);
            }
            let filled = (span.offset + span.stride * plane.rows) as u32;
            used[span.buffer] = used[span.buffer].max(filled.min(dst.len() as u32));
        }
        let memory = maps.len();
        let mut q = super::QueueBuffer::mmap(self.source.buf_type, index);
        q.timestamp = timestamp;
        q.planes = used[..memory]
            .iter()
            .map(|&bytes_used| super::QueuePlane {
                bytes_used,
                ..Default::default()
            })
            .collect();
        if let Err(e) = self.source.dev.queue(&q) {
            self.free.push_front(index);
            return Err(e);
        }
        Ok(true)
    }

    /// `frame`'s colour planes, each `(row_bytes, rows)` in `shape`, as `(bytes, stride)` at
    /// the destination format's layout (see [`Self::push_image`]). A plane past the buffer's
    /// end comes back empty.
    pub fn image_planes(&self, frame: &M2mFrame, shape: &[(usize, usize)]) -> Vec<(&[u8], usize)> {
        let maps = &self.dest.maps[frame.buffer.index as usize];
        spans(&self.dest_format, shape)
            .into_iter()
            .zip(shape)
            .map(|(span, &(row_bytes, rows))| {
                let end = span.offset + span.stride * rows.saturating_sub(1) + row_bytes;
                let data = maps
                    .get(span.buffer)
                    .and_then(|m| m.as_slice().get(span.offset..end))
                    .unwrap_or(&[]);
                (data, span.stride)
            })
            .collect()
    }

    /// The next filled destination buffer, waiting up to `timeout`; `None` if none came.
    pub fn pull(&mut self, timeout: Duration) -> Result<Option<M2mFrame>> {
        let got = self.dest.dequeue_within(timeout)?;
        self.reap()?;
        Ok(got.map(|buffer| M2mFrame { buffer }))
    }

    /// The bytes of `frame`'s plane `plane`.
    pub fn data(&self, frame: &M2mFrame, plane: usize) -> &[u8] {
        let used = frame
            .buffer
            .planes
            .get(plane)
            .map_or(0, |&(used, _)| used as usize);
        self.dest.maps[frame.buffer.index as usize]
            .get(plane)
            .map_or(&[][..], |m| &m.as_slice()[..used.min(m.len())])
    }

    /// Gives `frame`'s buffer back to the device.
    pub fn release(&mut self, frame: M2mFrame) -> Result<()> {
        self.dest.queue(frame.buffer.index, &[])
    }

    /// Asks an encoder to finish: the frames queued so far are encoded and the last
    /// destination buffer comes back flagged `LAST` ([`M2mFrame::is_last`]).
    pub fn drain(&self) -> Result<()> {
        let mut cmd = v4l2_encoder_cmd {
            cmd: ENC_CMD_STOP,
            ..Default::default()
        };
        // SAFETY: VIDIOC_ENCODER_CMD takes a `v4l2_encoder_cmd`.
        unsafe { ioctl::ioctl(self.source.dev.as_fd(), raw::VIDIOC_ENCODER_CMD, &mut cmd) }?;
        Ok(())
    }

    /// Stops both queues and frees their buffers.
    pub fn close(self) -> Result<()> {
        let r = self.dest.close();
        self.source.close()?;
        r
    }

    /// Takes back the source buffers the device has finished reading, dropping what was held
    /// for them.
    fn reap(&mut self) -> Result<()> {
        while let Some(done) = self.source.try_dequeue()? {
            if let Some(slot) = self.held.get_mut(done.index as usize) {
                slot.hold = None;
            }
            self.free.push_back(done.index);
        }
        Ok(())
    }

    /// An error unless the source side has buffers to copy into.
    fn copies(&self) -> Result<()> {
        if self.imports() {
            return Err(Error::Invalid(
                "m2m source: copying into a session that imports dma-bufs".into(),
            ));
        }
        Ok(())
    }
}

/// The queue type for a format: `OUTPUT` for the source side, `CAPTURE` for the destination.
fn buf_type(format: &Format, source: bool) -> Result<BufType> {
    Ok(match (format, source) {
        (Format::Single(_), true) => BufType::VideoOutput,
        (Format::Single(_), false) => BufType::VideoCapture,
        (Format::Multi(_), true) => BufType::VideoOutputMplane,
        (Format::Multi(_), false) => BufType::VideoCaptureMplane,
        (Format::Meta(_), _) => {
            return Err(Error::Invalid("m2m: a metadata format".into()));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FourCc;
    use crate::v4l2::{CapabilityFlags, PixFormat};

    /// The first node whose driver is `driver` (a test driver loaded with `modprobe`).
    fn node(driver: &str) -> Option<VideoDevice> {
        crate::v4l2::list_video_nodes()
            .into_iter()
            .filter_map(|p| VideoDevice::open(p).ok())
            .find(|d| {
                let caps = d.capabilities();
                caps.driver == driver
                    && caps
                        .device_caps
                        .intersects(CapabilityFlags::VIDEO_M2M | CapabilityFlags::VIDEO_M2M_MPLANE)
            })
    }

    fn pix(width: u32, height: u32, fourcc: FourCc) -> Format {
        Format::Single(PixFormat {
            width,
            height,
            fourcc,
            ..Default::default()
        })
    }

    #[test]
    fn colour_planes_follow_each_other_in_one_buffer() {
        let nv12 = Format::Single(PixFormat {
            width: 100,
            height: 10,
            bytes_per_line: 128,
            ..Default::default()
        });
        let s = spans(&nv12, &[(100, 10), (100, 5)]);
        assert_eq!(
            s,
            [
                Span {
                    buffer: 0,
                    offset: 0,
                    stride: 128
                },
                Span {
                    buffer: 0,
                    offset: 1280,
                    stride: 128
                },
            ]
        );
        // YU12: the chroma planes at half the luma stride.
        let s = spans(&nv12, &[(100, 10), (50, 5), (50, 5)]);
        assert_eq!(
            s[1],
            Span {
                buffer: 0,
                offset: 1280,
                stride: 64
            }
        );
        assert_eq!(
            s[2],
            Span {
                buffer: 0,
                offset: 1600,
                stride: 64
            }
        );
    }

    #[test]
    fn colour_planes_take_a_buffer_each_when_there_are_enough() {
        let multi = Format::Multi(crate::v4l2::PixFormatMplane {
            width: 100,
            height: 10,
            planes: vec![
                crate::v4l2::PlaneFormat {
                    bytes_per_line: 112,
                    size_image: 1120,
                },
                crate::v4l2::PlaneFormat {
                    bytes_per_line: 0,
                    size_image: 560,
                },
            ],
            ..Default::default()
        });
        let s = spans(&multi, &[(100, 10), (100, 5)]);
        assert_eq!(
            s[0],
            Span {
                buffer: 0,
                offset: 0,
                stride: 112
            }
        );
        // A stride the driver left at 0 is the row width.
        assert_eq!(
            s[1],
            Span {
                buffer: 1,
                offset: 0,
                stride: 100
            }
        );
    }

    #[test]
    fn metadata_formats_have_no_m2m_queue() {
        let meta = Format::Meta(Default::default());
        assert!(buf_type(&meta, true).is_err());
        assert_eq!(
            buf_type(&pix(2, 2, FourCc::new(b"YUYV")), true).unwrap(),
            BufType::VideoOutput
        );
    }

    /// vim2m (`modprobe vim2m`): RGB24 frames through the scaler come back the same size,
    /// stamped with their source frame's time. Skipped without the module.
    #[test]
    fn vim2m_round_trips_frames() {
        let Some(dev) = node("vim2m") else {
            eprintln!("skipped: no vim2m node (modprobe vim2m)");
            return;
        };
        let rgb = FourCc::new(b"RGB3");
        let (w, h) = (64, 48);
        let mut s = M2mSession::open(dev, &pix(w, h, rgb), &pix(w, h, rgb), 2).unwrap();
        let frame: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
        for k in 0..4u64 {
            assert!(s.push(&[&frame], Duration::from_millis(k)).unwrap());
            let out = s.pull(Duration::from_secs(2)).unwrap().expect("a frame");
            assert_eq!(out.buffer.timestamp, Duration::from_millis(k));
            assert_eq!(s.data(&out, 0).len(), frame.len());
            s.release(out).unwrap();
        }
        s.close().unwrap();
    }

    /// vim2m importing dma-bufs from the system heap: each frame is read in place, comes back
    /// the same, and the frame held for a slot is dropped once the device gives it back.
    /// Skipped without the module or the heap.
    #[test]
    fn vim2m_reads_dmabufs_in_place() {
        use crate::dma_heap::{Access, DmaHeap};
        use std::rc::Rc;
        let (Some(dev), Ok(heap)) = (node("vim2m"), DmaHeap::open("system")) else {
            eprintln!("skipped: no vim2m node or system dma-heap");
            return;
        };
        let rgb = FourCc::new(b"RGB3");
        let (w, h) = (64u32, 48u32);
        let len = (w * h * 3) as usize;
        let mut s: M2mSession<Rc<()>> =
            M2mSession::open_importing(dev, &pix(w, h, rgb), &pix(w, h, rgb), 2).unwrap();
        assert!(s.imports());
        assert!(s.push(&[&[0u8; 4][..]], Duration::ZERO).is_err());
        let bufs: Vec<_> = (0..2).map(|_| heap.allocate(len).unwrap()).collect();
        let held = Rc::new(());
        for k in 0..6u64 {
            let buf = &bufs[k as usize % 2];
            let mut map = buf.map().unwrap();
            buf.begin_cpu_access(Access::Write).unwrap();
            for (i, b) in map.as_mut_slice()[..len].iter_mut().enumerate() {
                *b = ((i as u64 + k * 7) % 251) as u8;
            }
            buf.end_cpu_access(Access::Write).unwrap();
            let plane = DmabufSource {
                fd: buf.as_fd(),
                offset: 0,
                bytes: len as u32,
            };
            assert!(
                s.push_dmabuf(&[plane], Duration::from_millis(k), held.clone())
                    .unwrap()
            );
            let out = s.pull(Duration::from_secs(2)).unwrap().expect("a frame");
            assert_eq!(out.buffer.timestamp, Duration::from_millis(k));
            assert_eq!(s.data(&out, 0), &map.as_slice()[..len], "frame {k}");
            s.release(out).unwrap();
        }
        // Taking the slots back drops what was held for them.
        let _ = s.can_push().unwrap();
        assert!(Rc::strong_count(&held) <= 2, "{}", Rc::strong_count(&held));
        s.close().unwrap();
    }
}
