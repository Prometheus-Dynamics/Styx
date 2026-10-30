//! Running the vendor `rkisp` driver's nodes through `styx-kernel` (feature `device`).
//!
//! ```text
//! sensor ─► (rkcif sditf / csi) ─► rkisp-isp-subdev ─► rkisp_mainpath  (NV12)
//!                     rkisp-input-params ─►│  └─► rkisp-statistics (3A statistics)
//! ```
//!
//! [`IspDevice`] routes whatever feeds the ISP's sink pad through it into the main path,
//! sets the pad and node formats, and keeps three queues: parameter buffers in
//! ([`Params`]), statistics and output frames out. The entity names and pad numbers are the
//! vendor kernel's (`drivers/media/platform/rockchip/isp`); nothing here has run on an RK3588
//! yet.

use std::path::PathBuf;
use std::string::String;
use std::time::{Duration, Instant};
use std::vec::Vec;
use std::{fmt, format};

use styx_kernel::FourCc;
use styx_kernel::Mapping;
use styx_kernel::media::{LinkFlags, MediaDevice, PadRef, Topology};
use styx_kernel::subdev::{MbusCode, MbusFormat, Subdev, Which};
use styx_kernel::v4l2::{
    BufType, DequeuedBuffer, Format, Memory, MetaFormat, PixFormatMplane, QueueBuffer, QueuePlane,
    VideoDevice,
};

use crate::Params;

/// The ISP subdevice.
pub const ISP_ENTITY: &str = "rkisp-isp-subdev";
/// The parameter node (meta output).
pub const PARAMS_ENTITY: &str = "rkisp-input-params";
/// The statistics node (meta capture).
pub const STATS_ENTITY: &str = "rkisp-statistics";
/// The main path's output node.
pub const MAINPATH_ENTITY: &str = "rkisp_mainpath";

/// ISP pads: raw in, parameters in, image out, statistics out.
const PAD_SINK: u32 = 0;
const PAD_PARAMS: u32 = 1;
const PAD_SOURCE: u32 = 2;
const PAD_STATS: u32 = 3;

/// `V4L2_META_FMT_RK_ISP1_PARAMS`: the vendor driver's parameter format (the ISP30 layout).
pub const PARAMS_FOURCC: FourCc = FourCc::new(b"RK1P");
/// `V4L2_META_FMT_RK_ISP1_STAT_3A`.
pub const STATS_FOURCC: FourCc = FourCc::new(b"RK1S");
/// The main path's output: NV12.
pub const OUTPUT_FOURCC: FourCc = FourCc::new(b"NV12");
/// `MEDIA_BUS_FMT_YUYV8_2X8`: the ISP's image source pad.
const MBUS_YUYV8_2X8: MbusCode = MbusCode(0x2008);

/// A device error.
#[derive(Debug)]
#[non_exhaustive]
pub enum DeviceError {
    /// A kernel call failed.
    Kernel(styx_kernel::Error),
    /// The graph or a format is not what is needed.
    Setup(String),
    /// Nothing arrived in time.
    Timeout(&'static str),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kernel(e) => write!(f, "{e}"),
            Self::Setup(s) => write!(f, "setup: {s}"),
            Self::Timeout(what) => write!(f, "timed out waiting for {what}"),
        }
    }
}

impl std::error::Error for DeviceError {}

impl From<styx_kernel::Error> for DeviceError {
    fn from(e: styx_kernel::Error) -> Self {
        Self::Kernel(e)
    }
}

/// Result alias.
pub type Result<T> = core::result::Result<T, DeviceError>;

/// Media devices with an rkisp ISP subdevice ([`ISP_ENTITY`]), in path order (one per ISP
/// instance, `rkisp-vir0`, `rkisp-vir1`, ...).
pub fn find_isps() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = styx_kernel::media::list_media_devices()
        .into_iter()
        .filter(|p| {
            styx_kernel::media::MediaDevice::open_read_only(p)
                .and_then(|m| Ok((m.device_info()?, m.topology()?)))
                .is_ok_and(|(i, t)| {
                    i.driver.starts_with("rkisp") && t.entity_by_name(ISP_ENTITY).is_some()
                })
        })
        .collect();
    v.sort();
    v
}

/// What to set up.
#[derive(Clone, Copy, Debug)]
pub struct IspSetup {
    /// Sensor mode width.
    pub width: u32,
    /// Sensor mode height.
    pub height: u32,
    /// The raw media bus code the ISP takes (the sensor's, e.g. `SGRBG10_1X10`).
    pub sensor_code: MbusCode,
    /// The main path's output size (the driver scales).
    pub output: (u32, u32),
    /// Buffers per queue.
    pub buffers: u32,
}

/// An output frame left dequeued by [`IspDevice::next_output`]; give it back with
/// [`IspDevice::release`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputFrame {
    /// Buffer index.
    pub index: u32,
    /// Frame sequence.
    pub sequence: u32,
    /// Capture timestamp (`CLOCK_MONOTONIC`).
    pub timestamp: Duration,
    /// Payload bytes.
    pub bytes_used: usize,
    /// The driver flagged the frame.
    pub error: bool,
}

/// An MMAP buffer queue on one node.
struct Queue {
    dev: VideoDevice,
    buf_type: BufType,
    maps: Vec<Vec<Mapping>>,
    name: &'static str,
}

impl Queue {
    fn new(dev: VideoDevice, buf_type: BufType, count: u32, name: &'static str) -> Result<Self> {
        let got = dev.request_buffers(buf_type, Memory::Mmap, count)?;
        if got.count == 0 {
            return Err(DeviceError::Setup(format!("{name}: no buffers")));
        }
        let maps = (0..got.count)
            .map(|i| dev.map_buffer(buf_type, i))
            .collect::<styx_kernel::Result<Vec<_>>>()?;
        Ok(Self {
            dev,
            buf_type,
            maps,
            name,
        })
    }

    fn len(&self) -> u32 {
        self.maps.len() as u32
    }

    fn queue(&self, index: u32, bytes_used: u32) -> Result<()> {
        let mut q = QueueBuffer::mmap(self.buf_type, index);
        if self.buf_type.is_multiplanar() || bytes_used > 0 {
            q.planes = styx_kernel::v4l2::Planes::one(QueuePlane {
                bytes_used,
                ..Default::default()
            });
        }
        Ok(self.dev.queue(&q)?)
    }

    fn try_dequeue(&self) -> Result<Option<DequeuedBuffer>> {
        Ok(self.dev.dequeue(self.buf_type, Memory::Mmap)?)
    }

    fn dequeue(&self, timeout: Duration) -> Result<DequeuedBuffer> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(b) = self.try_dequeue()? {
                return Ok(b);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(DeviceError::Timeout(self.name));
            }
            self.dev.wait(Some(left))?;
        }
    }

    fn close(mut self) -> Result<()> {
        let r = self.dev.stream_off(self.buf_type);
        self.maps.clear();
        let f = self.dev.free_buffers(self.buf_type, Memory::Mmap);
        r?;
        f?;
        Ok(())
    }
}

fn entity(t: &Topology, name: &str) -> Result<u32> {
    t.entity_by_name(name)
        .map(|e| e.id)
        .ok_or_else(|| DeviceError::Setup(format!("no '{name}' entity")))
}

fn devnode(t: &Topology, id: u32, what: &str) -> Result<PathBuf> {
    t.devnode_path(id)
        .ok_or_else(|| DeviceError::Setup(format!("no device node for '{what}'")))
}

/// The ISP path, set up and holding its buffers.
pub struct IspDevice {
    _media: MediaDevice,
    params: Queue,
    stats: Queue,
    output: Queue,
    /// Parameter buffers not queued.
    free_params: Vec<u32>,
    /// The main path's line stride and size.
    stride: u32,
    output_size: (u32, u32),
    output_path: PathBuf,
    streaming: bool,
}

impl IspDevice {
    /// Opens the ISP of media device `media` (from [`find_isps`]): links whatever feeds the
    /// ISP's sink pad, the parameter node, the main path and the statistics node; sets the
    /// formats; allocates buffers.
    pub fn open(media: impl AsRef<std::path::Path>, setup: &IspSetup) -> Result<Self> {
        let media = MediaDevice::open(media)?;
        let t = media.topology()?;
        let isp = entity(&t, ISP_ENTITY)?;
        let params = entity(&t, PARAMS_ENTITY)?;
        let stats = entity(&t, STATS_ENTITY)?;
        let main = entity(&t, MAINPATH_ENTITY)?;
        let source = t
            .links_to(isp)
            .into_iter()
            .find(|l| l.sink.index == PAD_SINK)
            .ok_or_else(|| DeviceError::Setup(format!("nothing feeds {ISP_ENTITY}:0")))?;
        let pad = |entity, index| PadRef { entity, index };
        let wanted = [
            (source.source, pad(isp, PAD_SINK)),
            (pad(params, 0), pad(isp, PAD_PARAMS)),
            (pad(isp, PAD_SOURCE), pad(main, 0)),
            (pad(isp, PAD_STATS), pad(stats, 0)),
        ];
        for (s, k) in wanted {
            let link = t
                .data_links()
                .into_iter()
                .find(|l| l.source == s && l.sink == k);
            match link {
                Some(l) if l.flags.contains(LinkFlags::ENABLED) => {}
                Some(_) => media.setup_link(s, k, LinkFlags::ENABLED)?,
                None => {
                    return Err(DeviceError::Setup(
                        "a link the path needs is missing".into(),
                    ));
                }
            }
        }

        // The raw format along the way in: the source's (its sensor configured by the camera)
        // must match the mode.
        let raw = MbusFormat {
            width: setup.width,
            height: setup.height,
            code: setup.sensor_code,
            field: 1,
            ..Default::default()
        };
        let upstream = Subdev::open(devnode(&t, source.source.entity, "the ISP's source")?)?;
        let mut got = upstream.format(source.source.index, Which::Active)?;
        if (got.width, got.height, got.code) != (raw.width, raw.height, raw.code) {
            got = upstream.set_format(source.source.index, Which::Active, &raw)?;
        }
        if (got.width, got.height, got.code) != (raw.width, raw.height, raw.code) {
            return Err(DeviceError::Setup(format!(
                "the ISP's source gives {}x{} code {:#x}",
                got.width, got.height, got.code.0
            )));
        }
        let sd = Subdev::open(devnode(&t, isp, ISP_ENTITY)?)?;
        sd.set_format(PAD_SINK, Which::Active, &got)?;
        let yuv = MbusFormat {
            code: MBUS_YUYV8_2X8,
            ..got
        };
        let out = sd.set_format(PAD_SOURCE, Which::Active, &yuv)?;
        if out.code != MBUS_YUYV8_2X8 {
            return Err(DeviceError::Setup(
                "the ISP refused its YUV source code".into(),
            ));
        }

        let meta = |node: u32, ty: BufType, fourcc: FourCc, name: &'static str| -> Result<Queue> {
            let dev = VideoDevice::open(devnode(&t, node, name)?)?;
            dev.set_format(
                ty,
                &Format::Meta(MetaFormat {
                    fourcc,
                    ..Default::default()
                }),
            )?;
            Queue::new(dev, ty, setup.buffers, name)
        };
        let params = meta(params, BufType::MetaOutput, PARAMS_FOURCC, PARAMS_ENTITY)?;
        let stats = meta(stats, BufType::MetaCapture, STATS_FOURCC, STATS_ENTITY)?;
        let output_path = devnode(&t, main, MAINPATH_ENTITY)?;
        let dev = VideoDevice::open(&output_path)?;
        let f = dev.set_format(
            BufType::VideoCaptureMplane,
            &Format::Multi(PixFormatMplane {
                width: setup.output.0,
                height: setup.output.1,
                fourcc: OUTPUT_FOURCC,
                field: 1,
                ..Default::default()
            }),
        )?;
        let Format::Multi(p) = f else {
            return Err(DeviceError::Setup(
                "the main path is not multi-planar".into(),
            ));
        };
        if p.fourcc != OUTPUT_FOURCC || p.planes.len() != 1 {
            return Err(DeviceError::Setup(format!(
                "the main path gave {} in {} planes",
                p.fourcc,
                p.planes.len()
            )));
        }
        let stride = p.planes[0].bytes_per_line;
        let output = Queue::new(
            dev,
            BufType::VideoCaptureMplane,
            setup.buffers,
            MAINPATH_ENTITY,
        )?;
        Ok(Self {
            free_params: (0..params.len()).rev().collect(),
            _media: media,
            params,
            stats,
            output,
            stride,
            output_size: (p.width, p.height),
            output_path,
            streaming: false,
        })
    }

    /// The main path's output size.
    pub fn output_size(&self) -> (u32, u32) {
        self.output_size
    }

    /// The main path's node (for the camera's frame starts while the ISP owns the stream).
    pub fn output_path(&self) -> &std::path::Path {
        &self.output_path
    }

    /// The main path's line stride in bytes (the chroma plane follows the luma plane).
    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// Queues `first` (the parameters for the first frames), every statistics and output
    /// buffer, and starts streaming (the main path last: the sensor starts with it).
    pub fn start(&mut self, first: &Params) -> Result<()> {
        if !self.queue_params(first)? {
            return Err(DeviceError::Setup("no parameter buffer".into()));
        }
        for i in 0..self.stats.len() {
            self.stats.queue(i, 0)?;
        }
        for i in 0..self.output.len() {
            self.output.queue(i, 0)?;
        }
        self.params.dev.stream_on(self.params.buf_type)?;
        self.stats.dev.stream_on(self.stats.buf_type)?;
        self.output.dev.stream_on(self.output.buf_type)?;
        self.streaming = true;
        Ok(())
    }

    /// Queues a parameter buffer; `false` when every buffer is still with the driver (the
    /// parameters were not queued; the ISP keeps the last ones it took).
    pub fn queue_params(&mut self, params: &Params) -> Result<bool> {
        while let Some(b) = self.params.try_dequeue()? {
            self.free_params.push(b.index);
        }
        let Some(i) = self.free_params.pop() else {
            return Ok(false);
        };
        let bytes = params.as_bytes();
        let map = &mut self.params.maps[i as usize][0];
        let Some(dst) = map.as_mut_slice().get_mut(..bytes.len()) else {
            self.free_params.push(i);
            return Err(DeviceError::Setup(format!(
                "a parameter buffer holds {} bytes, the parameters are {}",
                map.len(),
                bytes.len()
            )));
        };
        dst.copy_from_slice(bytes);
        self.params.queue(i, bytes.len() as u32)?;
        Ok(true)
    }

    /// Waits for the next statistics buffer and hands its bytes to `read`; the buffer goes
    /// back to the driver afterwards. Returns the frame's sequence, timestamp and `read`'s
    /// result.
    pub fn next_statistics<R>(
        &mut self,
        timeout: Duration,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<(u32, Duration, R)> {
        let b = self.stats.dequeue(timeout)?;
        let data = self.stats.maps[b.index as usize][0].as_slice();
        let r = read(&data[..b.bytes_used().min(data.len())]);
        self.stats.queue(b.index, 0)?;
        Ok((b.sequence, b.timestamp, r))
    }

    /// Waits for the next output frame and leaves it dequeued.
    pub fn next_output(&mut self, timeout: Duration) -> Result<OutputFrame> {
        let b = self.output.dequeue(timeout)?;
        Ok(OutputFrame {
            index: b.index,
            sequence: b.sequence,
            timestamp: b.timestamp,
            bytes_used: b.bytes_used(),
            error: b.flags.contains(styx_kernel::v4l2::BufferFlags::ERROR),
        })
    }

    /// An output frame's bytes (NV12 at [`Self::stride`]).
    pub fn output_data(&self, frame: &OutputFrame) -> &[u8] {
        let data = self.output.maps[frame.index as usize][0].as_slice();
        &data[..frame.bytes_used.min(data.len())]
    }

    /// Gives an output frame back to the driver.
    pub fn release(&self, frame: &OutputFrame) -> Result<()> {
        self.output.queue(frame.index, 0)
    }

    /// Stops streaming and frees the buffers.
    pub fn close(self) -> Result<()> {
        // The main path first: the sensor stops with it.
        let o = self.output.close();
        let s = self.stats.close();
        let p = self.params.close();
        o.and(s).and(p)
    }

    /// Whether [`Self::start`] has run.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }
}
