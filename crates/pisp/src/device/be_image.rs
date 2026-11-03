//! One `pispbe` node group running [`crate::be::ops`] jobs on the caller's dma-bufs: an
//! image in, one output or both, the config fixed at open. Written from the driver's node API as
//! [`super::BackEndStream`] drives it; untested on the board.

use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::media::MediaDevice;
use styx_kernel::v4l2::{
    BufType, BufferFlags, Format, MetaFormat, MmapQueue, Planes, QueueBuffer, QueuePlane,
};

use super::be::{BE_CFG_FOURCC, node};
use super::be_stream::mplane;
use super::{DeviceError, Queue, Result, find_media};
use crate::be::ops::BeOp;
use crate::format::formats;
use crate::uapi::{BeTilesConfig, ImageFormatConfig};

/// A node group set up for one op: the input and outputs import the caller's dma-bufs.
pub struct BeImageDevice {
    _media: MediaDevice,
    input: MmapQueue,
    output: MmapQueue,
    /// Output 1 of an op writing both ([`BeOp::out1`]), and its size.
    output1: Option<(MmapQueue, u32)>,
    config: Queue,
    cfg_len: u32,
    input_len: u32,
    output_len: u32,
}

/// Sets `f` (strides zero: the driver picks) on `dev` and returns it with the driver's
/// strides, and the plane's size.
fn negotiate(
    dev: &styx_kernel::v4l2::VideoDevice,
    buf_type: BufType,
    f: ImageFormatConfig,
    fourcc: [u8; 4],
    name: &str,
) -> Result<(ImageFormatConfig, u32)> {
    let fourcc = FourCc::new(&fourcc);
    let got = dev.set_format(
        buf_type,
        &mplane(u32::from(f.width), u32::from(f.height), fourcc, 0),
    )?;
    let Format::Multi(g) = got else {
        return Err(DeviceError::Setup(format!("{name} is not multi-planar")));
    };
    if g.fourcc != fourcc || (g.width, g.height) != (u32::from(f.width), u32::from(f.height)) {
        return Err(DeviceError::Setup(format!(
            "{name} gave {} {}x{}",
            g.fourcc, g.width, g.height
        )));
    }
    let plane = g.planes[0];
    let stride = plane.bytes_per_line as i32;
    let stride2 = match f.format {
        formats::YUV420P => stride / 2,
        formats::NV12 => stride,
        _ => 0,
    };
    Ok((
        ImageFormatConfig {
            stride,
            stride2,
            ..f
        },
        plane.size_image,
    ))
}

impl BeImageDevice {
    /// Opens node group `group` (0 or 1) for `op`: sets its formats on the nodes, writes the
    /// strides they chose into `op`, prepares the config and starts streaming. The input and
    /// output dma-bufs given to [`Self::run`] must hold [`Self::input_len`] and
    /// [`Self::output_len`] bytes; an op writing both outputs runs with [`Self::run2`].
    pub fn open(group: usize, op: &mut BeOp) -> Result<Self> {
        let path = find_media("pispbe")
            .into_iter()
            .nth(group)
            .ok_or_else(|| DeviceError::Setup(format!("no pispbe node group {group}")))?;
        let media = MediaDevice::open(path)?;
        let t = media.topology()?;
        let in_dev = node(&t, "pispbe-input")?;
        let (input, input_len) = negotiate(
            &in_dev,
            BufType::VideoOutputMplane,
            op.input,
            op.input_fourcc,
            "pispbe-input",
        )?;
        let out_name = if op.output == 0 {
            "pispbe-output0"
        } else {
            "pispbe-output1"
        };
        let out_dev = node(&t, out_name)?;
        let (out, output_len) = negotiate(
            &out_dev,
            BufType::VideoCaptureMplane,
            op.out,
            op.out_fourcc,
            out_name,
        )?;
        op.set_formats(input, out);
        let mut output1 = None;
        if let Some(out1) = op.out1 {
            let dev = node(&t, "pispbe-output1")?;
            let (out1, len) = negotiate(
                &dev,
                BufType::VideoCaptureMplane,
                out1,
                op.out_fourcc,
                "pispbe-output1",
            )?;
            op.set_out1_format(out1);
            output1 = Some((dev, len));
        }
        let cfg = op
            .backend
            .prepare()
            .map_err(|e| DeviceError::Config(e.to_string()))?;
        let cfg_dev = node(&t, "pispbe-config")?;
        cfg_dev.set_format(
            BufType::MetaOutput,
            &Format::Meta(MetaFormat {
                fourcc: BE_CFG_FOURCC,
                ..Default::default()
            }),
        )?;
        let mut config = Queue::new(cfg_dev, BufType::MetaOutput, 1, "pispbe-config")?;
        let bytes = BeTilesConfig::as_bytes(&cfg);
        config.maps[0][0].as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
        let timer = super::profile::record_if_on;
        let s = Self {
            input: MmapQueue::importing(in_dev, BufType::VideoOutputMplane, 1, "pispbe-input")?
                .with_timer(timer),
            output: MmapQueue::importing(out_dev, BufType::VideoCaptureMplane, 1, out_name)?
                .with_timer(timer),
            output1: match output1 {
                Some((dev, len)) => Some((
                    MmapQueue::importing(dev, BufType::VideoCaptureMplane, 1, "pispbe-output1")?
                        .with_timer(timer),
                    len,
                )),
                None => None,
            },
            cfg_len: bytes.len() as u32,
            config,
            input_len,
            output_len,
            _media: media,
        };
        s.input.stream_on()?;
        s.output.stream_on()?;
        if let Some((q, _)) = &s.output1 {
            q.stream_on()?;
        }
        s.config.stream_on()?;
        Ok(s)
    }

    /// The bytes an input dma-buf must hold.
    pub fn input_len(&self) -> u32 {
        self.input_len
    }

    /// The bytes an output dma-buf must hold.
    pub fn output_len(&self) -> u32 {
        self.output_len
    }

    /// The bytes an output 1 dma-buf must hold, for an op writing both outputs.
    pub fn output1_len(&self) -> Option<u32> {
        self.output1.as_ref().map(|(_, len)| *len)
    }

    /// Runs the op once: `input` read, `output` written (both dma-bufs, imported per job,
    /// nothing copied or allocated). Returns the time from queueing the job to its output.
    pub fn run(
        &mut self,
        input: BorrowedFd<'_>,
        output: BorrowedFd<'_>,
        timeout: Duration,
    ) -> Result<Duration> {
        if self.output1.is_some() {
            return Err(DeviceError::Setup(
                "the op writes both outputs: use run2".into(),
            ));
        }
        self.run_job(input, output, None, timeout)
    }

    /// [`Self::run`] for an op writing both outputs: `output` gets output 0, `output1`
    /// output 1, from the one job.
    pub fn run2(
        &mut self,
        input: BorrowedFd<'_>,
        output: BorrowedFd<'_>,
        output1: BorrowedFd<'_>,
        timeout: Duration,
    ) -> Result<Duration> {
        if self.output1.is_none() {
            return Err(DeviceError::Setup(
                "the op writes one output: use run".into(),
            ));
        }
        self.run_job(input, output, Some(output1), timeout)
    }

    fn run_job(
        &mut self,
        input: BorrowedFd<'_>,
        output: BorrowedFd<'_>,
        output1: Option<BorrowedFd<'_>>,
        timeout: Duration,
    ) -> Result<Duration> {
        let plane = |fd, len, used| {
            Planes::one(QueuePlane {
                dmabuf: Some(fd),
                length: len,
                bytes_used: used,
                data_offset: 0,
            })
        };
        let mut q = QueueBuffer::dmabuf(BufType::VideoCaptureMplane, 0, &[output]);
        q.planes = plane(output, self.output_len, 0);
        self.output.dev.queue(&q)?;
        if let (Some(fd), Some((queue, len))) = (output1, &self.output1) {
            let mut q = QueueBuffer::dmabuf(BufType::VideoCaptureMplane, 0, &[fd]);
            q.planes = plane(fd, *len, 0);
            queue.dev.queue(&q)?;
        }
        let mut q = QueueBuffer::dmabuf(BufType::VideoOutputMplane, 0, &[input]);
        q.planes = plane(input, self.input_len, self.input_len);
        self.input.dev.queue(&q)?;
        let start = Instant::now();
        self.config.queue(0, &[self.cfg_len])?;
        let done = self
            .output
            .dequeue_within(timeout)?
            .ok_or(DeviceError::Timeout(self.output.name));
        let done1 = match &mut self.output1 {
            Some((q, _)) => Some(match q.dequeue_within(timeout) {
                Ok(d) => d.ok_or(DeviceError::Timeout(q.name)),
                Err(e) => Err(e.into()),
            }),
            None => None,
        };
        let elapsed = start.elapsed();
        // Take the input and config back whatever the outputs did, so the next run can queue.
        let a = self.input.dequeue_within(timeout);
        let b = self.config.dequeue(timeout);
        let done = done?;
        if let Some(d) = done1
            && d?.flags.contains(BufferFlags::ERROR)
        {
            return Err(DeviceError::Setup("pispbe returned an error buffer".into()));
        }
        a?.ok_or(DeviceError::Timeout(self.input.name))?;
        b?;
        if done.flags.contains(BufferFlags::ERROR) {
            return Err(DeviceError::Setup("pispbe returned an error buffer".into()));
        }
        Ok(elapsed)
    }

    /// Stops streaming and frees the buffers.
    pub fn stop(self) -> Result<()> {
        let a = self.config.close();
        let b = self.input.close();
        let c = self.output.close();
        let d = self.output1.map_or(Ok(()), |(q, _)| q.close());
        a.and(b.map_err(Into::into))
            .and(c.map_err(Into::into))
            .and(d.map_err(Into::into))
    }
}
