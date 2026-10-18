//! The front end in the `rp1-cfe` media graph.

use std::os::fd::OwnedFd;
use std::time::Duration;

use styx_kernel::FourCc;
use styx_kernel::media::{LinkFlags, MediaDevice, PadRef, Topology};
use styx_kernel::subdev::{MbusCode, MbusFormat, Subdev, Which};
use styx_kernel::v4l2::{BufType, Format, MetaFormat, PixFormat, SelectionTarget, VideoDevice};

use super::{DeviceError, Queue, Result, bayer16_fourcc, find_media};
use crate::fe::FrontEnd;
use crate::stats::Statistics;
use crate::uapi::{BayerOrder, FeConfig, ImageFormatConfig, RawStatistics, image_format};

/// `MEDIA_BUS_FMT_SBGGR16_1X16` and friends.
fn mbus16(order: BayerOrder) -> MbusCode {
    MbusCode(match order {
        BayerOrder::Bggr => 0x301d,
        BayerOrder::Gbrg => 0x301e,
        BayerOrder::Grbg => 0x301f,
        BayerOrder::Rggb => 0x3020,
        BayerOrder::Greyscale => 0x202e,
    })
}

/// `V4L2_META_FMT_RPI_FE_CFG`.
pub const FE_CFG_FOURCC: FourCc = FourCc::new(b"RPFC");
/// `V4L2_META_FMT_RPI_FE_STATS`.
pub const FE_STATS_FOURCC: FourCc = FourCc::new(b"RPFS");

/// What to set up.
#[derive(Clone, Copy, Debug)]
pub struct FrontEndSetup {
    /// Sensor mode width.
    pub width: u32,
    /// Sensor mode height.
    pub height: u32,
    /// Sensor media bus code (e.g. `SBGGR10_1X10`).
    pub sensor_code: MbusCode,
    /// Bayer order of the sensor data.
    pub bayer: BayerOrder,
    /// Also capture the front end's raw output 0 (`rp1-cfe-fe_image0`).
    pub image_output: bool,
    /// Buffers per queue.
    pub buffers: u32,
    /// Leave the receiver's embedded data link (`csi2` → `rp1-cfe-embedded`) as it is
    /// (whoever reads the sensor's embedded data enables it and streams that node).
    pub keep_embedded: bool,
}

/// One frame's worth of front end results.
#[derive(Debug)]
pub struct FeFrame {
    /// Statistics buffer sequence.
    pub sequence: u32,
    /// Statistics timestamp.
    pub timestamp: Duration,
    /// Decoded statistics.
    pub stats: Statistics,
    /// Raw image `(sequence, bytes)` from output 0, if enabled.
    pub raw: Option<(u32, Vec<u8>)>,
}

/// A raw frame left dequeued by [`FrontEndDevice::next_held`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldImage {
    /// Buffer index (also the index of its dma-buf in [`FrontEndDevice::image_dmabufs`]).
    pub index: u32,
    /// Frame sequence.
    pub sequence: u32,
    /// Capture timestamp (`CLOCK_MONOTONIC`).
    pub timestamp: Duration,
    /// Payload bytes.
    pub bytes_used: usize,
    /// The receiver flagged the frame.
    pub error: bool,
}

/// One frame's statistics and its raw frame, held.
#[derive(Debug)]
pub struct FeHeld {
    /// Statistics buffer sequence.
    pub sequence: u32,
    /// Statistics timestamp.
    pub timestamp: Duration,
    /// Decoded statistics.
    pub stats: Statistics,
    /// The raw frame, if the raw output is enabled.
    pub image: Option<HeldImage>,
}

/// One frame's raw frame, held, from [`FrontEndDevice::next_held_raw`] (its statistics are
/// in the caller's buffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeHeldRaw {
    /// Statistics buffer sequence.
    pub sequence: u32,
    /// Statistics timestamp.
    pub timestamp: Duration,
    /// The raw frame, if the raw output is enabled.
    pub image: Option<HeldImage>,
}

/// The front end path, set up and holding its buffers.
/// The front end's line event (`STYX_CFE_EVENT_LINES`, Styx's rp1-cfe module): frame
/// `sequence`'s raw output has `line` rows in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineEvent {
    /// The frame's sequence (its buffer's).
    pub sequence: u32,
    /// Rows in memory.
    pub line: u32,
    /// When (`CLOCK_MONOTONIC`).
    pub at: Duration,
    /// The frame's start: the timestamp its buffer will carry (`CLOCK_MONOTONIC`).
    pub frame_start: Duration,
}

/// The part of the sensor's field a mode reads, when it reads a window of it (a mode of
/// fewer lines at full pitch, e.g. the OV9782's 1280x480 in Styx's ov9282 module): its
/// position and size in the field's coordinates (the full mode's frame, e.g. 1280x800).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SensorWindow {
    /// Left edge in the field.
    pub x: u32,
    /// Top edge in the field.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// The field's size.
    pub field: (u32, u32),
}

impl SensorWindow {
    /// The sensor's crop against its crop bounds (`VIDIOC_SUBDEV_G_SELECTION`) on `pad`, as set
    /// for the current mode; `None` for a mode of the whole field (binned modes included) or a
    /// driver without the selection API.
    pub fn read(sd: &Subdev, pad: u32) -> Option<Self> {
        let crop = sd
            .selection(pad, Which::Active, SelectionTarget::Crop)
            .ok()?;
        let bounds = sd
            .selection(pad, Which::Active, SelectionTarget::CropBounds)
            .ok()?;
        Self::from_rects(
            (crop.left, crop.top, crop.width, crop.height),
            (bounds.left, bounds.top, bounds.width, bounds.height),
        )
    }

    /// From the crop and the crop bounds as `(left, top, width, height)`.
    pub fn from_rects(crop: (i32, i32, u32, u32), bounds: (i32, i32, u32, u32)) -> Option<Self> {
        if (crop.2, crop.3) == (bounds.2, bounds.3) || crop.2 == 0 || crop.3 == 0 {
            return None;
        }
        let x = u32::try_from(crop.0.checked_sub(bounds.0)?).ok()?;
        let y = u32::try_from(crop.1.checked_sub(bounds.1)?).ok()?;
        (x + crop.2 <= bounds.2 && y + crop.3 <= bounds.3).then_some(Self {
            x,
            y,
            width: crop.2,
            height: crop.3,
            field: (bounds.2, bounds.3),
        })
    }
}

pub struct FrontEndDevice {
    _media: MediaDevice,
    /// The slice-timing debug probe (`STYX_PISP_FE_LINES_PROBE=1`).
    lines_probe: Option<super::lines_probe::LinesProbe>,
    /// Raw buffers queued to the front end, oldest (being written or next) first.
    queued: std::cell::RefCell<std::collections::VecDeque<u32>>,
    /// Subscribed to the line event (Styx's rp1-cfe module): [`Self::enable_line_events`].
    line_events: bool,
    sensor_window: Option<SensorWindow>,
    stats: Queue,
    config: Queue,
    image: Option<Queue>,
    image_format: ImageFormatConfig,
    image_path: Option<std::path::PathBuf>,
    free_configs: Vec<u32>,
}

fn entity(t: &Topology, name: &str) -> Result<u32> {
    t.entity_by_name(name)
        .map(|e| e.id)
        .ok_or_else(|| DeviceError::Setup(format!("no '{name}' entity")))
}

fn node(t: &Topology, name: &str) -> Result<VideoDevice> {
    let id = entity(t, name)?;
    let path = t
        .devnode_path(id)
        .ok_or_else(|| DeviceError::Setup(format!("no device node for '{name}'")))?;
    Ok(VideoDevice::open(path)?)
}

fn subdev(t: &Topology, id: u32) -> Result<Subdev> {
    let path = t
        .devnode_path(id)
        .ok_or_else(|| DeviceError::Setup(format!("no subdev node for entity {id}")))?;
    Ok(Subdev::open(path)?)
}

impl FrontEndDevice {
    /// Finds `rp1-cfe`, routes the sensor through `csi2` into `pisp-fe` (disabling every other
    /// mutable link), sets the pad and node formats and allocates buffers.
    pub fn open(setup: &FrontEndSetup) -> Result<Self> {
        let path = find_media("rp1-cfe")
            .into_iter()
            .next()
            .ok_or_else(|| DeviceError::Setup("no rp1-cfe media device".into()))?;
        let media = MediaDevice::open(path)?;
        let t = media.topology()?;
        let csi2 = entity(&t, "csi2")?;
        let fe = entity(&t, "pisp-fe")?;
        let cfg_node = entity(&t, "rp1-cfe-fe_config")?;
        let stats_node = entity(&t, "rp1-cfe-fe_stats")?;
        let image_node = entity(&t, "rp1-cfe-fe_image0")?;
        let sensor = t
            .links_to(csi2)
            .into_iter()
            .find(|l| l.sink.index == 0)
            .ok_or_else(|| DeviceError::Setup("nothing feeds csi2:0".into()))?
            .source;

        let pad = |entity, index| PadRef { entity, index };
        let mut wanted = vec![
            (pad(csi2, 4), pad(fe, 0)),
            (pad(cfg_node, 0), pad(fe, 1)),
            (pad(fe, 4), pad(stats_node, 0)),
        ];
        if setup.image_output {
            wanted.push((pad(fe, 2), pad(image_node, 0)));
        }
        let embedded = t.entity_by_name("rp1-cfe-embedded").map(|e| e.id);
        for l in t.data_links() {
            let want = wanted.contains(&(l.source, l.sink))
                || (setup.keep_embedded && Some(l.sink.entity) == embedded);
            if l.flags.contains(LinkFlags::ENABLED)
                && !want
                && !l.flags.contains(LinkFlags::IMMUTABLE)
            {
                media.setup_link(l.source, l.sink, LinkFlags(0))?;
            }
        }
        for (s, k) in &wanted {
            media.setup_link(*s, *k, LinkFlags::ENABLED)?;
        }

        let sensor_sd = subdev(&t, sensor.entity)?;
        let mut sf = sensor_sd.format(sensor.index, Which::Active)?;
        sf.width = setup.width;
        sf.height = setup.height;
        sf.code = setup.sensor_code;
        let sf = sensor_sd.set_format(sensor.index, Which::Active, &sf)?;
        if (sf.width, sf.height, sf.code) != (setup.width, setup.height, setup.sensor_code) {
            return Err(DeviceError::Setup(format!(
                "sensor gave {}x{} code {:#x}",
                sf.width, sf.height, sf.code.0
            )));
        }
        let sensor_window = SensorWindow::read(&sensor_sd, sensor.index);
        let csi = subdev(&t, csi2)?;
        csi.set_format(0, Which::Active, &sf)?;
        let code16 = mbus16(setup.bayer);
        let f16 = MbusFormat { code: code16, ..sf };
        let got = csi.set_format(4, Which::Active, &f16)?;
        if got.code != code16 {
            return Err(DeviceError::Setup(
                "csi2 refused the 16-bit source code".into(),
            ));
        }
        let fesd = subdev(&t, fe)?;
        fesd.set_format(0, Which::Active, &f16)?;
        fesd.set_format(2, Which::Active, &f16)?;

        let stats_dev = node(&t, "rp1-cfe-fe_stats")?;
        stats_dev.set_format(
            BufType::MetaCapture,
            &Format::Meta(MetaFormat {
                fourcc: FE_STATS_FOURCC,
                ..Default::default()
            }),
        )?;
        let cfg_dev = node(&t, "rp1-cfe-fe_config")?;
        cfg_dev.set_format(
            BufType::MetaOutput,
            &Format::Meta(MetaFormat {
                fourcc: FE_CFG_FOURCC,
                ..Default::default()
            }),
        )?;
        let mut image_format = ImageFormatConfig {
            width: setup.width as u16,
            height: setup.height as u16,
            format: image_format::BPS_16,
            ..Default::default()
        };
        let image_path = t.devnode_path(image_node);
        let image = if setup.image_output {
            let dev = node(&t, "rp1-cfe-fe_image0")?;
            let f = dev.set_format(
                BufType::VideoCapture,
                &Format::Single(PixFormat {
                    width: setup.width,
                    height: setup.height,
                    fourcc: bayer16_fourcc(setup.bayer),
                    field: 1,
                    ..Default::default()
                }),
            )?;
            let Format::Single(p) = f else {
                return Err(DeviceError::Setup("fe_image0 is not single-planar".into()));
            };
            if p.fourcc != bayer16_fourcc(setup.bayer) {
                return Err(DeviceError::Setup(format!("fe_image0 gave {}", p.fourcc)));
            }
            image_format.stride = p.bytes_per_line as i32;
            Some(Queue::new(
                dev,
                BufType::VideoCapture,
                setup.buffers,
                "fe_image0",
            )?)
        } else {
            None
        };
        let lines_probe = match (&image, &image_path) {
            (Some(q), Some(path)) => super::lines_probe::LinesProbe::start(
                path,
                q.maps
                    .iter()
                    .filter_map(|m| m.first())
                    .map(|m| (m.as_ptr(), m.len()))
                    .collect(),
                image_format.stride as usize,
            ),
            _ => None,
        };
        Ok(Self {
            lines_probe,
            queued: Default::default(),
            line_events: false,
            stats: Queue::new(stats_dev, BufType::MetaCapture, setup.buffers, "fe_stats")?,
            config: Queue::new(cfg_dev, BufType::MetaOutput, setup.buffers, "fe_config")?,
            image,
            image_format,
            image_path,
            free_configs: Vec::new(),
            _media: media,
            sensor_window,
        })
    }

    /// The window of the sensor's field the mode reads ([`SensorWindow`]), if it reads one.
    pub fn sensor_window(&self) -> Option<SensorWindow> {
        self.sensor_window
    }

    /// The format of the raw output node (for `FrontEnd::set_output_format(0, ..)` and as
    /// the back end's input).
    pub fn image_format(&self) -> ImageFormatConfig {
        self.image_format
    }

    fn queue_config(&mut self, index: u32, cfg: &FeConfig) -> Result<()> {
        let bytes = cfg.as_bytes();
        super::config_dump::fe_config(bytes);
        self.config.maps[index as usize][0].as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
        self.config.queue(index, &[bytes.len() as u32])
    }

    fn feed_configs(&mut self, fe: &mut FrontEnd) -> Result<()> {
        while let Some(i) = self.free_configs.pop() {
            let cfg = super::profile::time("fe_config", "prepare", || fe.prepare())
                .map_err(DeviceError::Config)?;
            self.queue_config(i, &cfg)?;
        }
        Ok(())
    }

    /// Queues every capture buffer and `configs` config buffers, then starts streaming (the
    /// sensor starts when the last node starts).
    pub fn start(&mut self, fe: &mut FrontEnd, configs: usize) -> Result<()> {
        for i in 0..self.stats.len() as u32 {
            self.stats.queue(i, &[])?;
        }
        if let Some(q) = &self.image {
            for i in 0..q.len() as u32 {
                q.queue(i, &[])?;
                self.queued.borrow_mut().push_back(i);
            }
        }
        let n = configs.clamp(1, self.config.len()) as u32;
        for i in 0..n {
            let cfg = fe.prepare().map_err(DeviceError::Config)?;
            self.queue_config(i, &cfg)?;
        }
        self.free_configs = (n..self.config.len() as u32).rev().collect();
        self.config.stream_on()?;
        self.stats.stream_on()?;
        if let Some(q) = &self.image {
            q.stream_on()?;
        }
        Ok(())
    }

    /// Waits for the next statistics buffer (and raw frame), keeps the config queue fed.
    pub fn next_frame(&mut self, fe: &mut FrontEnd, timeout: Duration) -> Result<FeFrame> {
        let b = self.stats.dequeue(timeout)?;
        let stats = Statistics::parse(self.stats.maps[b.index as usize][0].as_slice())
            .map_err(|e| DeviceError::Setup(e.to_string()))?;
        self.stats.queue(b.index, &[])?;
        let raw = match &self.image {
            Some(q) => {
                let r = q.dequeue(timeout)?;
                self.queued.borrow_mut().retain(|i| *i != r.index);
                let data = q.maps[r.index as usize][0].as_slice()[..r.bytes_used()].to_vec();
                q.queue(r.index, &[])?;
                self.queued.borrow_mut().push_back(r.index);
                Some((r.sequence, data))
            }
            None => None,
        };
        while let Some(c) = self
            .config
            .dev
            .dequeue(self.config.buf_type, styx_kernel::v4l2::Memory::Mmap)?
        {
            self.free_configs.push(c.index);
        }
        self.feed_configs(fe)?;
        Ok(FeFrame {
            sequence: b.sequence,
            timestamp: b.timestamp,
            stats,
            raw,
        })
    }

    /// The statistics node's descriptor (readable when a frame's statistics are ready), for
    /// a caller that waits on it together with other descriptors before
    /// [`Self::next_held_raw`].
    pub fn stats_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.stats.dev.as_fd()
    }

    /// The raw output node (`rp1-cfe-fe_image0`); it sends the frame-start events.
    pub fn image_node_path(&self) -> Option<&std::path::Path> {
        self.image_path.as_deref()
    }

    /// Exports every raw output buffer as a dma-buf (in buffer index order), e.g. to import
    /// them into the back end's input queue.
    pub fn image_dmabufs(&self) -> Result<Vec<OwnedFd>> {
        let q = self
            .image
            .as_ref()
            .ok_or_else(|| DeviceError::Setup("no raw output".into()))?;
        (0..q.len() as u32)
            .map(|i| Ok(q.dev.export_buffer(q.buf_type, i, 0)?))
            .collect()
    }

    /// Bytes of raw output buffer `index`.
    pub fn image_data(&self, index: u32) -> Option<&[u8]> {
        let q = self.image.as_ref()?;
        Some(q.maps.get(index as usize)?.first()?.as_slice())
    }

    /// Waits for the next statistics buffer and, with the raw output enabled, the next raw
    /// frame, which stays dequeued (and is not copied) until [`Self::release_image`]. Keeps the
    /// config queue fed from `fe` (so changes to `fe` reach the frames a few configs later).
    pub fn next_held(&mut self, fe: &mut FrontEnd, timeout: Duration) -> Result<FeHeld> {
        let mut raw: Box<RawStatistics> = bytemuck::allocation::zeroed_box();
        let f = self.next_held_raw(fe, timeout, Some(&mut raw))?;
        Ok(FeHeld {
            sequence: f.sequence,
            timestamp: f.timestamp,
            stats: Statistics::from_raw(&raw),
            image: f.image,
        })
    }

    /// [`Self::next_held`] with the statistics buffer copied into `stats` as it is (one copy
    /// out of the uncached buffer, no decoding, no allocation), or not read at all (`None`).
    pub fn next_held_raw(
        &mut self,
        fe: &mut FrontEnd,
        timeout: Duration,
        stats: Option<&mut RawStatistics>,
    ) -> Result<FeHeldRaw> {
        let b = self.stats.dequeue(timeout)?;
        let n = size_of::<RawStatistics>();
        let src = self.stats.maps[b.index as usize][0].as_slice();
        if src.len() < n {
            return Err(DeviceError::Setup(format!(
                "statistics buffer is {} bytes, expected {n}",
                src.len()
            )));
        }
        if let Some(stats) = stats {
            super::profile::time("fe_stats", "copy", || {
                bytemuck::bytes_of_mut(stats).copy_from_slice(&src[..n]);
            });
        }
        self.stats.queue(b.index, &[])?;
        let image = match &self.image {
            Some(q) => {
                let r = q.dequeue(timeout)?;
                self.queued.borrow_mut().retain(|i| *i != r.index);
                if let (Some(p), Some(m)) =
                    (&mut self.lines_probe, q.maps[r.index as usize].first())
                {
                    p.check(
                        r.sequence,
                        r.index,
                        m.as_slice(),
                        self.image_format.stride as usize,
                    );
                }
                Some(HeldImage {
                    index: r.index,
                    sequence: r.sequence,
                    timestamp: r.timestamp,
                    bytes_used: r.bytes_used(),
                    error: r.flags.contains(styx_kernel::v4l2::BufferFlags::ERROR),
                })
            }
            None => None,
        };
        while let Some(c) = super::profile::time("fe_config", "dqbuf", || {
            self.config
                .dev
                .dequeue(self.config.buf_type, styx_kernel::v4l2::Memory::Mmap)
        })? {
            self.free_configs.push(c.index);
        }
        self.feed_configs(fe)?;
        Ok(FeHeldRaw {
            sequence: b.sequence,
            timestamp: b.timestamp,
            image,
        })
    }

    /// Gives a raw frame from [`Self::next_held`] back to the front end.
    pub fn release_image(&self, index: u32) -> Result<()> {
        match &self.image {
            Some(q) => {
                q.queue(index, &[])?;
                self.queued.borrow_mut().push_back(index);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// The raw buffer the front end writes now (or next): the oldest one queued. A line event
    /// (see [`Self::enable_line_events`]) is about this buffer's frame.
    pub fn image_in_flight(&self) -> Option<u32> {
        self.queued.borrow().front().copied()
    }

    /// Subscribes to the front end's line event on the raw output (Styx's rp1-cfe module,
    /// `STYX_CFE_EVENT_LINES`); fails with the stock driver. Poll [`Self::line_event_fd`] for
    /// priority and read them with [`Self::take_line_event`].
    pub fn enable_line_events(&mut self) -> Result<()> {
        use styx_kernel::event::{EventType, Events, SubscribeFlags};
        let q = self
            .image
            .as_ref()
            .ok_or_else(|| DeviceError::Setup("no raw output".into()))?;
        q.dev.subscribe(
            EventType::Private(super::LINES_EVENT_OFFSET),
            0,
            SubscribeFlags::empty(),
        )?;
        self.line_events = true;
        Ok(())
    }

    /// The raw output node's descriptor: priority when a line event is pending.
    pub fn line_event_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.line_events
            .then(|| self.image.as_ref().map(|q| q.dev.as_fd()))
            .flatten()
    }

    /// The next pending line event. `None` when none is pending.
    pub fn take_line_event(&self) -> Option<LineEvent> {
        use styx_kernel::event::{EventKind, Events};
        let q = self.image.as_ref()?;
        while let Ok(Some(ev)) = q.dev.dequeue_event() {
            if let EventKind::Private { data, .. } = ev.kind {
                let u32_at = |o: usize| u32::from_ne_bytes(data[o..o + 4].try_into().unwrap());
                let u64_at = |o: usize| u64::from_ne_bytes(data[o..o + 8].try_into().unwrap());
                return Some(LineEvent {
                    sequence: u32_at(0),
                    line: u32_at(4),
                    at: Duration::from_nanos(u64_at(8)),
                    frame_start: Duration::from_nanos(u64_at(16)),
                });
            }
        }
        None
    }

    /// Stops streaming and frees the buffers.
    pub fn stop(self) -> Result<()> {
        let mut first = None;
        if let Some(q) = self.image
            && let Err(e) = q.close()
        {
            first.get_or_insert(e);
        }
        if let Err(e) = self.stats.close() {
            first.get_or_insert(e);
        }
        if let Err(e) = self.config.close() {
            first.get_or_insert(e);
        }
        first.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod window_tests {
    use super::SensorWindow;

    /// What the ov9282 driver reports: crop bounds 1280x800 at (8, 8); Styx's 1280x480 mode
    /// crops array rows 168..647.
    #[test]
    fn a_windowed_mode_is_placed_in_the_full_field() {
        let bounds = (8, 8, 1280, 800);
        let w = SensorWindow::from_rects((8, 168, 1280, 480), bounds).unwrap();
        assert_eq!(
            (w.x, w.y, w.width, w.height, w.field),
            (0, 160, 1280, 480, (1280, 800))
        );
        // The full mode, and a binned one (640x400 reading the whole 1280x800): no window.
        assert_eq!(SensorWindow::from_rects((8, 8, 1280, 800), bounds), None);
        // The stock 1280x720 mode: the top 720 lines.
        let top = SensorWindow::from_rects((8, 8, 1280, 720), bounds).unwrap();
        assert_eq!((top.x, top.y, top.height), (0, 0, 720));
        // Outside the bounds: not trusted.
        assert_eq!(SensorWindow::from_rects((8, 400, 1280, 480), bounds), None);
        assert_eq!(SensorWindow::from_rects((0, 168, 1280, 480), bounds), None);
    }
}
