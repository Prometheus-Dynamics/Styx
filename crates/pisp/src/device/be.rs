//! One `pispbe` node group, memory to memory.

use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::media::{MediaDevice, Topology};
use styx_kernel::v4l2::{BufType, Format, MetaFormat, PixFormatMplane, PlaneFormat, VideoDevice};

use super::{DeviceError, Queue, Result, bayer16_fourcc, find_media};
use crate::format::formats;
use crate::uapi::{BayerOrder, BeTilesConfig, ImageFormatConfig};

/// `V4L2_META_FMT_RPI_BE_CFG`.
pub const BE_CFG_FOURCC: FourCc = FourCc::new(b"RPBC");

/// Output 0 format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeOutput {
    /// `V4L2_PIX_FMT_NV12` (one buffer, CbCr plane after Y).
    Nv12,
    /// `V4L2_PIX_FMT_RGB24` (R, G, B bytes).
    Rgb24,
}

impl BeOutput {
    fn fourcc(self) -> FourCc {
        match self {
            Self::Nv12 => FourCc::new(b"NV12"),
            Self::Rgb24 => FourCc::new(b"RGB3"),
        }
    }

    /// The PiSP format.
    pub fn pisp_format(self) -> u32 {
        match self {
            Self::Nv12 => formats::NV12,
            Self::Rgb24 => formats::RGB888,
        }
    }
}

/// The back end nodes, set up with buffers.
pub struct BackEndDevice {
    _media: MediaDevice,
    input: Queue,
    output: Queue,
    config: Queue,
    output_format: ImageFormatConfig,
}

fn node(t: &Topology, name: &str) -> Result<VideoDevice> {
    let id = t
        .entity_by_name(name)
        .ok_or_else(|| DeviceError::Setup(format!("no '{name}' entity")))?
        .id;
    let path = t
        .devnode_path(id)
        .ok_or_else(|| DeviceError::Setup(format!("no device node for '{name}'")))?;
    Ok(VideoDevice::open(path)?)
}

fn mplane(w: u16, h: u16, fourcc: FourCc, bpl: u32) -> Format {
    Format::Multi(PixFormatMplane {
        width: u32::from(w),
        height: u32::from(h),
        fourcc,
        field: 1,
        planes: vec![PlaneFormat {
            bytes_per_line: bpl,
            size_image: 0,
        }],
        ..Default::default()
    })
}

impl BackEndDevice {
    /// Opens node group `group` (0 or 1) for a 16-bit Bayer `input` and output 0 at the same
    /// size in `out`, allocates one buffer per node and starts streaming.
    pub fn open(
        group: usize,
        input: ImageFormatConfig,
        bayer: BayerOrder,
        out: BeOutput,
    ) -> Result<Self> {
        let path = find_media("pispbe")
            .into_iter()
            .nth(group)
            .ok_or_else(|| DeviceError::Setup(format!("no pispbe node group {group}")))?;
        let media = MediaDevice::open(path)?;
        let t = media.topology()?;
        let in_dev = node(&t, "pispbe-input")?;
        let got = in_dev.set_format(
            BufType::VideoOutputMplane,
            &mplane(
                input.width,
                input.height,
                bayer16_fourcc(bayer),
                input.stride as u32,
            ),
        )?;
        let Format::Multi(g) = got else {
            return Err(DeviceError::Setup(
                "pispbe-input is not multi-planar".into(),
            ));
        };
        if g.planes.first().map(|p| p.bytes_per_line) != Some(input.stride as u32) {
            return Err(DeviceError::Setup(format!(
                "pispbe-input stride {:?}",
                g.planes
            )));
        }
        let out_dev = node(&t, "pispbe-output0")?;
        let got = out_dev.set_format(
            BufType::VideoCaptureMplane,
            &mplane(input.width, input.height, out.fourcc(), 0),
        )?;
        let Format::Multi(g) = got else {
            return Err(DeviceError::Setup(
                "pispbe-output0 is not multi-planar".into(),
            ));
        };
        if g.fourcc != out.fourcc() {
            return Err(DeviceError::Setup(format!(
                "pispbe-output0 gave {}",
                g.fourcc
            )));
        }
        let stride = g.planes[0].bytes_per_line as i32;
        let output_format = ImageFormatConfig {
            width: g.width as u16,
            height: g.height as u16,
            format: out.pisp_format(),
            stride,
            stride2: if out == BeOutput::Nv12 { stride } else { 0 },
        };
        let cfg_dev = node(&t, "pispbe-config")?;
        cfg_dev.set_format(
            BufType::MetaOutput,
            &Format::Meta(MetaFormat {
                fourcc: BE_CFG_FOURCC,
                ..Default::default()
            }),
        )?;
        let s = Self {
            input: Queue::new(in_dev, BufType::VideoOutputMplane, 1, "pispbe-input")?,
            output: Queue::new(out_dev, BufType::VideoCaptureMplane, 1, "pispbe-output0")?,
            config: Queue::new(cfg_dev, BufType::MetaOutput, 1, "pispbe-config")?,
            output_format,
            _media: media,
        };
        s.input.stream_on()?;
        s.output.stream_on()?;
        s.config.stream_on()?;
        Ok(s)
    }

    /// Output 0's format as the V4L2 node set it (use it in the back end config).
    pub fn output_format(&self) -> ImageFormatConfig {
        self.output_format
    }

    /// Runs one job: `raw` in, `cfg` as the config; returns output 0's bytes and the time
    /// from queueing the job to its completion.
    pub fn process(
        &mut self,
        raw: &[u8],
        cfg: &BeTilesConfig,
        timeout: Duration,
    ) -> Result<(Vec<u8>, Duration)> {
        let dst = self.input.maps[0][0].as_mut_slice();
        if raw.len() > dst.len() {
            return Err(DeviceError::Setup(format!(
                "raw frame {} bytes, input buffer {}",
                raw.len(),
                dst.len()
            )));
        }
        dst[..raw.len()].copy_from_slice(raw);
        let bytes = cfg.as_bytes();
        self.config.maps[0][0].as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
        self.output.queue(0, &[])?;
        self.input.queue(0, &[raw.len() as u32])?;
        let start = Instant::now();
        self.config.queue(0, &[bytes.len() as u32])?;
        let done = self.output.dequeue(timeout)?;
        let elapsed = start.elapsed();
        self.input.dequeue(timeout)?;
        self.config.dequeue(timeout)?;
        if done.flags.contains(styx_kernel::v4l2::BufferFlags::ERROR) {
            return Err(DeviceError::Setup("pispbe returned an error buffer".into()));
        }
        let out = self.output.maps[0][0].as_slice()[..done.bytes_used()].to_vec();
        Ok((out, elapsed))
    }

    /// Stops streaming and frees the buffers.
    pub fn stop(self) -> Result<()> {
        let a = self.config.close();
        let b = self.input.close();
        let c = self.output.close();
        a.and(b).and(c)
    }
}
