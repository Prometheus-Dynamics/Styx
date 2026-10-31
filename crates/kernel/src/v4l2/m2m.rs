//! A memory-to-memory device session ([`M2mSession`]): a scaler or converter (vim2m, the
//! Rockchip RGA, the K230's 2D unit) or a stateful encoder (vicodec, the K230's VPU). Frames
//! are copied into the device's source (`OUTPUT`) buffers and come back in its destination
//! (`CAPTURE`) buffers, which the caller reads in place and gives back.

use std::collections::VecDeque;
use std::time::Duration;

use super::raw::{self, v4l2_encoder_cmd};
use super::{BufType, BufferFlags, DequeuedBuffer, Format, MmapQueue, VideoDevice};
use crate::{Error, Result, ioctl};
use std::os::fd::AsFd;

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
}

/// One stream through a memory-to-memory device, MMAP buffers on both sides.
pub struct M2mSession {
    source: MmapQueue,
    dest: MmapQueue,
    /// Source buffers not queued.
    free: VecDeque<u32>,
    source_format: Format,
    dest_format: Format,
}

impl M2mSession {
    /// Opens a stream on `dev` from frames of `source` to `dest`, with `buffers` buffers on
    /// each side (the driver may give more or fewer, at least one). The destination format is
    /// set first, as stateful encoders want; each side's queue is multi-planar when its format
    /// is. Every destination buffer is queued and both queues stream.
    pub fn open(dev: VideoDevice, source: &Format, dest: &Format, buffers: u32) -> Result<Self> {
        let (source_type, dest_type) = (buf_type(source, true)?, buf_type(dest, false)?);
        let dest_format = dev.set_format(dest_type, dest)?;
        let source_format = dev.set_format(source_type, source)?;
        // An encoder sizes its coded buffers from the source format: read it back.
        let dest_format = dev.format(dest_type).unwrap_or(dest_format);
        let dest_dev = dev.try_clone()?;
        let source = MmapQueue::new(dev, source_type, buffers.max(1), "m2m source")?;
        let dest = MmapQueue::new(dest_dev, dest_type, buffers.max(1), "m2m destination")?;
        for index in 0..dest.len() as u32 {
            dest.queue(index, &[])?;
        }
        let free = (0..source.len() as u32).collect();
        source.stream_on()?;
        dest.stream_on()?;
        Ok(Self {
            source,
            dest,
            free,
            source_format,
            dest_format,
        })
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

    /// Takes back the source buffers the device has finished reading.
    fn reap(&mut self) -> Result<()> {
        while let Some(done) = self.source.try_dequeue()? {
            self.free.push_back(done.index);
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
}
