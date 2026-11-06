//! A memory-to-memory device session ([`M2mSession`]): a scaler or converter (vim2m, the
//! Rockchip RGA, the K230's 2D unit) or a stateful encoder (vicodec, the K230's VPU). Frames
//! are copied into the device's source (`OUTPUT`) buffers, or handed to it by dma-buf
//! ([`M2mSession::open_importing`]), and come back in its destination (`CAPTURE`) buffers,
//! which the caller reads in place and gives back. Built on Lemnos's session
//! (`lemnos_linux::v4l2::m2m`), which owns the queues and ioctls.

use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd};
use std::time::Duration;

use lemnos_linux::v4l2 as lm;
use lemnos_linux::v4l2::m2m as lmm;

use lemnos_v4l2::v4l2::{BufferFlags, DequeuedBuffer, Format, VideoDevice};
use lemnos_v4l2::{Error, Result};

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

/// One stream through a memory-to-memory device, on Lemnos's session
/// (`lemnos_linux::v4l2::m2m`): MMAP destination buffers, and MMAP source buffers
/// ([`Self::open`]) or source slots importing the caller's dma-bufs ([`Self::open_importing`]).
/// `H` is what the caller keeps alive while the device reads a dma-buf (the frame over it),
/// dropped when the device gives the slot back.
pub struct M2mSession<H = ()> {
    session: lmm::M2mSession,
    /// The same open file (a duplicate descriptor): controls go through it.
    dev: VideoDevice,
    import: bool,
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
    /// each side (the driver may give more or fewer, at least one). The source format is set
    /// first, then the destination's, which is read back (an encoder sizes its coded buffers
    /// from the source); each side's queue is multi-planar when its format is. Every
    /// destination buffer is queued and both queues stream.
    pub fn open(dev: VideoDevice, source: &Format, dest: &Format, buffers: u32) -> Result<Self> {
        Self::open_with(dev, source, dest, buffers, false)
    }
}

/// A Lemnos call's error as Styx's.
fn lm_err(call: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Sys { call, source }
}

/// `f` checked for an m2m queue (`Meta` has none; at most [`lm::MAX_PLANES`] planes).
fn to_lemnos(f: &Format) -> Result<Format> {
    match f {
        Format::Multi(m) if m.planes.is_empty() || m.planes.len() > lm::MAX_PLANES => Err(
            Error::Invalid(format!("m2m: {} format planes", m.planes.len())),
        ),
        Format::Meta(_) => Err(Error::Invalid("m2m: a metadata format".into())),
        _ => Ok(f.clone()),
    }
}

/// The memory planes a buffer of `f` has.
fn num_planes(f: &Format) -> usize {
    match f {
        Format::Multi(m) => m.planes.len(),
        _ => 1,
    }
}

/// A plane of an MMAP buffer to queue, with `bytes_used` bytes of data.
fn mmap_plane(bytes_used: u32) -> lm::QueuePlane<'static> {
    lm::QueuePlane {
        memory: lm::PlaneMemory::Mmap,
        length: 0,
        bytes_used,
        data_offset: 0,
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
    /// `H`. Controls set on `dev` before this stay set (they belong to the open file).
    pub fn open_with(
        dev: VideoDevice,
        source: &Format,
        dest: &Format,
        buffers: u32,
        import: bool,
    ) -> Result<Self> {
        let setup = |format, memory| lmm::QueueSetup {
            format,
            memory,
            buffers: buffers.max(1),
            crop: None,
        };
        let source_memory = if import {
            lm::Memory::DmaBuf
        } else {
            lm::Memory::Mmap
        };
        let fd = dev.as_fd().try_clone_to_owned().map_err(lm_err("dup"))?;
        let mut session = lmm::M2mSession::from_file(
            fd,
            setup(to_lemnos(source)?, source_memory),
            setup(to_lemnos(dest)?, lm::Memory::Mmap),
        )
        .map_err(lm_err("m2m open"))?;
        let mmap = [mmap_plane(0); lm::MAX_PLANES];
        let planes = num_planes(session.capture_format());
        for index in 0..session.capture_buffers() {
            session
                .queue_capture(index, &mmap[..planes])
                .map_err(lm_err("VIDIOC_QBUF"))?;
        }
        session.stream_on().map_err(lm_err("VIDIOC_STREAMON"))?;
        let slots = session.output_buffers();
        Ok(Self {
            source_format: session.output_format().clone(),
            dest_format: session.capture_format().clone(),
            session,
            dev,
            import,
            free: (0..slots).collect(),
            held: (0..slots)
                .map(|_| Slot {
                    hold: None,
                    last: None,
                })
                .collect(),
        })
    }

    /// Whether the source side imports dma-bufs ([`Self::open_importing`]).
    pub fn imports(&self) -> bool {
        self.import
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
        if !self.import {
            return Err(Error::Invalid(
                "m2m source: push_dmabuf on a session opened without importing".into(),
            ));
        }
        let multi = matches!(self.session.output_format(), Format::Multi(_));
        if planes.is_empty()
            || planes.len() > lemnos_v4l2::v4l2::MAX_PLANES
            || (!multi && planes.len() != 1)
        {
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
        let key = lemnos_linux_sys::dma_buf::identity(planes[0].fd).ok();
        let at = self
            .free
            .iter()
            .position(|&i| key.is_some() && self.held[i as usize].last == key)
            .unwrap_or(0);
        let Some(index) = self.free.remove(at) else {
            return Ok(false);
        };
        let mut q = [mmap_plane(0); lm::MAX_PLANES];
        for (qp, p) in q.iter_mut().zip(planes) {
            // `bytesused` counts from the plane's start, `data_offset` included; a length of 0
            // takes the dma-buf's size.
            *qp = lm::QueuePlane {
                memory: lm::PlaneMemory::Dmabuf(p.fd),
                length: 0,
                bytes_used: p.offset.saturating_add(p.bytes),
                data_offset: p.offset,
            };
        }
        if let Err(e) = self
            .session
            .queue_output(index, &q[..planes.len()], timestamp)
        {
            self.free.push_front(index);
            return Err(lm_err("VIDIOC_QBUF")(e));
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

    /// The device node (the session's open file, duplicated): set controls through it.
    pub fn device(&self) -> &VideoDevice {
        &self.dev
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
        let count = num_planes(self.session.output_format());
        if planes.len() != count {
            self.free.push_front(index);
            return Err(Error::Invalid(format!(
                "m2m source: {} planes for a {count}-plane buffer",
                planes.len()
            )));
        }
        let mut q = [mmap_plane(0); lm::MAX_PLANES];
        for (p, plane) in planes.iter().enumerate() {
            let dst = self.session.output_plane_mut(index, p).unwrap_or_default();
            if plane.len() > dst.len() {
                let len = dst.len();
                self.free.push_front(index);
                return Err(Error::Invalid(format!(
                    "m2m source: plane {p} of {} bytes in a {len}-byte buffer",
                    plane.len()
                )));
            }
            dst[..plane.len()].copy_from_slice(plane);
            q[p].bytes_used = plane.len() as u32;
        }
        self.queue_source(index, &q[..count], timestamp)
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
        let mut shape = [(0usize, 0usize); lemnos_v4l2::v4l2::MAX_PLANES];
        for (s, p) in shape.iter_mut().zip(planes) {
            *s = (p.row_bytes, p.rows);
        }
        let layout = spans(
            &self.source_format,
            &shape[..planes.len().min(lemnos_v4l2::v4l2::MAX_PLANES)],
        );
        let memory = num_planes(self.session.output_format());
        let mut q = [mmap_plane(0); lm::MAX_PLANES];
        for (p, (plane, span)) in planes.iter().zip(&layout).enumerate() {
            let Some(dst) = self.session.output_plane_mut(index, span.buffer) else {
                self.free.push_front(index);
                return Err(Error::Invalid(format!(
                    "m2m source: plane {p} has no buffer plane"
                )));
            };
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
            let filled = ((span.offset + span.stride * plane.rows) as u32).min(dst.len() as u32);
            let used = &mut q[span.buffer].bytes_used;
            *used = (*used).max(filled);
        }
        self.queue_source(index, &q[..memory], timestamp)
    }

    /// Queues MMAP source buffer `index` (taken from `free`), giving it back on failure.
    fn queue_source(
        &mut self,
        index: u32,
        planes: &[lm::QueuePlane<'_>],
        timestamp: Duration,
    ) -> Result<bool> {
        if let Err(e) = self.session.queue_output(index, planes, timestamp) {
            self.free.push_front(index);
            return Err(lm_err("VIDIOC_QBUF")(e));
        }
        Ok(true)
    }

    /// `frame`'s colour planes, each `(row_bytes, rows)` in `shape`, as `(bytes, stride)` at
    /// the destination format's layout (see [`Self::push_image`]). A plane past the buffer's
    /// end comes back empty.
    pub fn image_planes(&self, frame: &M2mFrame, shape: &[(usize, usize)]) -> Vec<(&[u8], usize)> {
        spans(&self.dest_format, shape)
            .into_iter()
            .zip(shape)
            .map(|(span, &(row_bytes, rows))| {
                let end = span.offset + span.stride * rows.saturating_sub(1) + row_bytes;
                let data = self
                    .session
                    .capture_plane(frame.buffer.index, span.buffer)
                    .and_then(|m| m.get(span.offset..end))
                    .unwrap_or(&[]);
                (data, span.stride)
            })
            .collect()
    }

    /// The next filled destination buffer, waiting up to `timeout`; `None` if none came.
    pub fn pull(&mut self, timeout: Duration) -> Result<Option<M2mFrame>> {
        let mut got = self
            .session
            .dequeue_capture()
            .map_err(lm_err("VIDIOC_DQBUF"))?;
        if got.is_none() {
            let want = lmm::Ready {
                capture: true,
                ..Default::default()
            };
            let ready = self
                .session
                .wait(want, Some(timeout))
                .map_err(lm_err("poll"))?;
            if ready.capture {
                got = self
                    .session
                    .dequeue_capture()
                    .map_err(lm_err("VIDIOC_DQBUF"))?;
            }
        }
        self.reap()?;
        Ok(got.map(|d| M2mFrame {
            buffer: DequeuedBuffer {
                index: d.index,
                sequence: d.sequence,
                flags: BufferFlags(d.flags),
                field: 0,
                timestamp: d.timestamp,
                planes: d.bytes_used[..d.num_planes as usize]
                    .iter()
                    .map(|&used| (used, 0))
                    .collect(),
            },
        }))
    }

    /// The bytes of `frame`'s plane `plane`.
    pub fn data(&self, frame: &M2mFrame, plane: usize) -> &[u8] {
        let used = frame
            .buffer
            .planes
            .get(plane)
            .map_or(0, |&(used, _)| used as usize);
        self.session
            .capture_plane(frame.buffer.index, plane)
            .map_or(&[][..], |m| &m[..used.min(m.len())])
    }

    /// Gives `frame`'s buffer back to the device.
    pub fn release(&mut self, frame: M2mFrame) -> Result<()> {
        let mmap = [mmap_plane(0); lm::MAX_PLANES];
        let planes = num_planes(self.session.capture_format());
        self.session
            .queue_capture(frame.buffer.index, &mmap[..planes])
            .map_err(lm_err("VIDIOC_QBUF"))
    }

    /// Asks an encoder to finish: the frames queued so far are encoded and the last
    /// destination buffer comes back flagged `LAST` ([`M2mFrame::is_last`]).
    pub fn drain(&self) -> Result<()> {
        self.session
            .drain(lmm::Drain::Encoder)
            .map_err(lm_err("VIDIOC_ENCODER_CMD"))
    }

    /// Stops both queues and frees their buffers.
    pub fn close(mut self) -> Result<()> {
        self.session
            .stream_off()
            .map_err(lm_err("VIDIOC_STREAMOFF"))
    }

    /// Takes back the source buffers the device has finished reading, dropping what was held
    /// for them.
    fn reap(&mut self) -> Result<()> {
        while let Some(done) = self
            .session
            .dequeue_output()
            .map_err(lm_err("VIDIOC_DQBUF"))?
        {
            if let Some(slot) = self.held.get_mut(done.index as usize) {
                slot.hold = None;
            }
            self.free.push_back(done.index);
        }
        Ok(())
    }

    /// An error unless the source side has buffers to copy into.
    fn copies(&self) -> Result<()> {
        if self.import {
            return Err(Error::Invalid(
                "m2m source: copying into a session that imports dma-bufs".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::print_stderr)]
mod tests;
