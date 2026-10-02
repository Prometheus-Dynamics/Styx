//! One `pispbe` node group for a stream of frames: the input imports dma-bufs (the front
//! end's raw buffers, no copy), up to two outputs with their own sizes and formats, a fresh
//! config per job, and output buffers held by the caller until released.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::media::MediaDevice;
use styx_kernel::v4l2::{
    BufType, BufferFlags, Format, Memory, MetaFormat, QueueBuffer, QueuePlane,
};

use super::be::BE_CFG_FOURCC;
use super::{DeviceError, Queue, Result, bayer16_fourcc, find_media};
use crate::format::formats;
use crate::uapi::{BayerOrder, BeTilesConfig, ImageFormatConfig};

/// An output format of the back end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeFormat {
    /// `NV12`: Y plane then interleaved CbCr in one buffer.
    Nv12,
    /// `RGB3`: R, G, B bytes.
    Rgb24,
    /// `GREY`: 8-bit luma only.
    Grey,
}

impl BeFormat {
    /// The V4L2 pixel format.
    pub fn fourcc(self) -> FourCc {
        FourCc::new(match self {
            Self::Nv12 => b"NV12",
            Self::Rgb24 => b"RGB3",
            Self::Grey => b"GREY",
        })
    }

    /// The PiSP image format.
    pub fn pisp_format(self) -> u32 {
        match self {
            Self::Nv12 => formats::NV12,
            Self::Rgb24 => formats::RGB888,
            Self::Grey => crate::uapi::image_format::BPS_8,
        }
    }
}

/// One output: format and size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeOutputSetup {
    /// Format.
    pub format: BeFormat,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// A finished job: which output buffers hold the results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeJob {
    /// Output buffer index per output (`None` for an output that is not set up).
    pub outputs: [Option<u32>; 2],
    /// From queueing the config to output 0 (or 1) completing.
    pub elapsed: Duration,
}

struct Output {
    queue: Queue,
    format: ImageFormatConfig,
    dmabufs: Vec<OwnedFd>,
    free: Vec<u32>,
}

/// The back end set up for a stream. See the [module documentation](self).
pub struct BackEndStream {
    _media: MediaDevice,
    input: styx_kernel::v4l2::VideoDevice,
    inputs: Vec<OwnedFd>,
    input_len: u32,
    outputs: [Option<Output>; 2],
    config: Queue,
}

fn mplane(w: u32, h: u32, fourcc: FourCc, bpl: u32) -> Format {
    Format::Multi(styx_kernel::v4l2::PixFormatMplane {
        width: w,
        height: h,
        fourcc,
        field: 1,
        planes: vec![styx_kernel::v4l2::PlaneFormat {
            bytes_per_line: bpl,
            size_image: 0,
        }],
        ..Default::default()
    })
}

impl BackEndStream {
    /// Opens node group `group` for 16-bit Bayer `input` frames held in `inputs` (dma-bufs of
    /// `input_len` bytes, e.g. [`super::FrontEndDevice::image_dmabufs`]; input buffer `i` is
    /// `inputs[i]`), with `outputs` (at least output 0) and `buffers` buffers per output.
    pub fn open(
        group: usize,
        input: ImageFormatConfig,
        bayer: BayerOrder,
        inputs: Vec<OwnedFd>,
        input_len: u32,
        outputs: [Option<BeOutputSetup>; 2],
        buffers: u32,
    ) -> Result<Self> {
        if outputs[0].is_none() || inputs.is_empty() {
            return Err(DeviceError::Setup("need output 0 and input buffers".into()));
        }
        let path = find_media("pispbe")
            .into_iter()
            .nth(group)
            .ok_or_else(|| DeviceError::Setup(format!("no pispbe node group {group}")))?;
        let media = MediaDevice::open(path)?;
        let t = media.topology()?;
        let node = |name: &str| -> Result<styx_kernel::v4l2::VideoDevice> {
            let id = t
                .entity_by_name(name)
                .ok_or_else(|| DeviceError::Setup(format!("no '{name}' entity")))?
                .id;
            let p = t
                .devnode_path(id)
                .ok_or_else(|| DeviceError::Setup(format!("no device node for '{name}'")))?;
            Ok(styx_kernel::v4l2::VideoDevice::open(p)?)
        };
        let in_dev = node("pispbe-input")?;
        let fmt = mplane(
            u32::from(input.width),
            u32::from(input.height),
            bayer16_fourcc(bayer),
            input.stride as u32,
        );
        let Format::Multi(g) = in_dev.set_format(BufType::VideoOutputMplane, &fmt)? else {
            return Err(DeviceError::Setup(
                "pispbe-input is not multi-planar".into(),
            ));
        };
        if g.planes.first().map(|p| p.bytes_per_line) != Some(input.stride as u32) {
            return Err(DeviceError::Setup(format!(
                "pispbe-input planes {:?}",
                g.planes
            )));
        }
        let got = in_dev.request_buffers(
            BufType::VideoOutputMplane,
            Memory::DmaBuf,
            inputs.len() as u32,
        )?;
        if (got.count as usize) < inputs.len() {
            return Err(DeviceError::Setup(format!(
                "pispbe-input gave {} of {} buffers",
                got.count,
                inputs.len()
            )));
        }
        let mut outs: [Option<Output>; 2] = [None, None];
        for (i, setup) in outputs.iter().enumerate() {
            let Some(o) = setup else { continue };
            let dev = node(&format!("pispbe-output{i}"))?;
            let Format::Multi(g) = dev.set_format(
                BufType::VideoCaptureMplane,
                &mplane(o.width, o.height, o.format.fourcc(), 0),
            )?
            else {
                return Err(DeviceError::Setup(format!(
                    "pispbe-output{i} is not multi-planar"
                )));
            };
            if (g.width, g.height, g.fourcc) != (o.width, o.height, o.format.fourcc()) {
                return Err(DeviceError::Setup(format!(
                    "pispbe-output{i} gave {}x{} {}",
                    g.width, g.height, g.fourcc
                )));
            }
            let stride = g.planes[0].bytes_per_line as i32;
            let format = ImageFormatConfig {
                width: g.width as u16,
                height: g.height as u16,
                format: o.format.pisp_format(),
                stride,
                stride2: if o.format == BeFormat::Nv12 {
                    stride
                } else {
                    0
                },
            };
            let name = if i == 0 {
                "pispbe-output0"
            } else {
                "pispbe-output1"
            };
            let queue = Queue::new(dev, BufType::VideoCaptureMplane, buffers.max(2), name)?;
            let dmabufs = (0..queue.len() as u32)
                .map(|b| Ok(queue.dev.export_buffer(queue.buf_type, b, 0)?))
                .collect::<Result<Vec<_>>>()?;
            let free = (0..queue.len() as u32).rev().collect();
            outs[i] = Some(Output {
                queue,
                format,
                dmabufs,
                free,
            });
        }
        let cfg_dev = node("pispbe-config")?;
        cfg_dev.set_format(
            BufType::MetaOutput,
            &Format::Meta(MetaFormat {
                fourcc: BE_CFG_FOURCC,
                ..Default::default()
            }),
        )?;
        let config = Queue::new(cfg_dev, BufType::MetaOutput, 1, "pispbe-config")?;
        let s = Self {
            _media: media,
            input: in_dev,
            inputs,
            input_len,
            outputs: outs,
            config,
        };
        s.input.stream_on(BufType::VideoOutputMplane)?;
        for o in s.outputs.iter().flatten() {
            o.queue.stream_on()?;
        }
        s.config.stream_on()?;
        Ok(s)
    }

    /// Output `i`'s format as the node set it (put it in the config's output format).
    pub fn output_format(&self, i: usize) -> Option<ImageFormatConfig> {
        self.outputs.get(i)?.as_ref().map(|o| o.format)
    }

    /// Runs one job on input buffer `input` with `cfg`, waiting up to `timeout`. The output
    /// buffers stay with the caller until [`Self::release`]; fails when an output has no free
    /// buffer.
    pub fn process(&mut self, input: u32, cfg: &BeTilesConfig, timeout: Duration) -> Result<BeJob> {
        let bytes = cfg.as_bytes();
        self.config.maps[0][0].as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
        let fd = self
            .inputs
            .get(input as usize)
            .ok_or_else(|| DeviceError::Setup(format!("no input buffer {input}")))?;
        let mut picked = [None, None];
        for (i, o) in self.outputs.iter_mut().enumerate() {
            if let Some(o) = o {
                let b = o.free.pop().ok_or_else(|| {
                    DeviceError::Setup(format!("output {i}: every buffer is held"))
                })?;
                picked[i] = Some(b);
            }
        }
        let result = self.run(input, fd.as_fd(), picked, cfg, timeout);
        if result.is_err() {
            // Whatever completed is unusable; hand the buffers back.
            for (o, b) in self.outputs.iter_mut().zip(picked) {
                if let (Some(o), Some(b)) = (o, b) {
                    o.free.push(b);
                }
            }
        }
        result
    }

    fn run(
        &self,
        input: u32,
        fd: BorrowedFd<'_>,
        picked: [Option<u32>; 2],
        cfg: &BeTilesConfig,
        timeout: Duration,
    ) -> Result<BeJob> {
        let bytes = cfg.as_bytes();
        for (o, b) in self.outputs.iter().zip(picked) {
            if let (Some(o), Some(b)) = (o, b) {
                o.queue.queue(b, &[])?;
            }
        }
        let mut q = QueueBuffer::dmabuf(BufType::VideoOutputMplane, input, &[fd]);
        q.planes = vec![QueuePlane {
            dmabuf: Some(fd),
            length: self.input_len,
            bytes_used: self.input_len,
            data_offset: 0,
        }];
        self.input.queue(&q)?;
        let start = Instant::now();
        self.config.queue(0, &[bytes.len() as u32])?;
        let mut elapsed = None;
        let mut error = false;
        for o in self.outputs.iter().flatten() {
            let done = o.queue.dequeue(timeout)?;
            error |= done.flags.contains(BufferFlags::ERROR);
            elapsed.get_or_insert(start.elapsed());
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .input
                .dequeue(BufType::VideoOutputMplane, Memory::DmaBuf)?
                .is_some()
            {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(DeviceError::Timeout("pispbe-input"));
            }
            self.input.wait(Some(left))?;
        }
        self.config.dequeue(timeout)?;
        if error {
            return Err(DeviceError::Setup("pispbe returned an error buffer".into()));
        }
        Ok(BeJob {
            outputs: picked,
            elapsed: elapsed.unwrap_or_default(),
        })
    }

    /// The bytes of output `i`'s buffer `index`.
    pub fn output_data(&self, i: usize, index: u32) -> Option<&[u8]> {
        let o = self.outputs.get(i)?.as_ref()?;
        Some(o.queue.maps.get(index as usize)?.first()?.as_slice())
    }

    /// The dma-buf of output `i`'s buffer `index`.
    pub fn output_dmabuf(&self, i: usize, index: u32) -> Option<BorrowedFd<'_>> {
        let o = self.outputs.get(i)?.as_ref()?;
        o.dmabufs.get(index as usize).map(|f| f.as_fd())
    }

    /// Hands a job's output buffers back for reuse.
    pub fn release(&mut self, job: &BeJob) {
        for (o, b) in self.outputs.iter_mut().zip(job.outputs) {
            if let (Some(o), Some(b)) = (o, b)
                && !o.free.contains(&b)
            {
                o.free.push(b);
            }
        }
    }

    /// Stops streaming and frees the buffers.
    pub fn stop(self) -> Result<()> {
        let a = self.config.close();
        let b = self.input.stream_off(BufType::VideoOutputMplane);
        let c = self
            .input
            .free_buffers(BufType::VideoOutputMplane, Memory::DmaBuf);
        let mut first = a.err();
        for o in self.outputs.into_iter().flatten() {
            drop(o.dmabufs);
            if let Err(e) = o.queue.close() {
                first.get_or_insert(e);
            }
        }
        b?;
        c?;
        first.map_or(Ok(()), Err)
    }
}
