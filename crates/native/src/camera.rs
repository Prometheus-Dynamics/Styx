//! An open bridged camera: power and mode, the receiver path, buffers, streaming and controls.
//!
//! ```text
//! open        bridge, I²C address, sensor driver; exclusive (advisory lock on the bridge)
//! configure   power, chip id, init, mode (sensor in standby), bridge format and timing,
//!             media links, receiver pads, video format, the initial frame duration
//! start       event thread (acks + FRAME_SYNC), buffers, STREAMON -> FrameStream
//! stop        STREAMOFF (the sensor stops on the bridge's request), buffers go with the frames
//! drop        stop, standby, power down
//! ```

use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use styx_graph::Fraction;
use styx_graph::rt::AsyncFd;
use styx_kernel::FourCc;
use styx_kernel::bus::i2c::{AddrWidth, I2cDevice};
use styx_kernel::bus::{PadFormat, SensorBridge, Timing as BridgeTiming};
use styx_kernel::event::{EventType, Events, SubscribeFlags};
use styx_kernel::media::{LinkFlags, MediaDevice};
use styx_kernel::subdev::{MbusCode, MbusFormat, Subdev, Which};
use styx_kernel::v4l2::{BufType, Format, PixFormat, VideoDevice};
use styx_sensor::{ControlRequest, DriverState, SensorDriver, Step};

use crate::buffers::{BufferMemory, BufferSet, Lender};
use crate::control::{BlankingHook, ControlHandle, ExpectedStart, SensorControl, lock};
use crate::discover::{CameraInfo, find_media};
use crate::embedded::{EmbeddedCapture, embedded_link};
use crate::error::{KernelContext, NativeError, Result};
use crate::events::EventThread;
use crate::formats;
use crate::modes::{SensorMode, interval_duration};
use crate::regbus::{BridgePins, I2cRegisterBus};
use crate::stream::{FrameStream, SensorSide, StreamShared};
use crate::topology::{apply_plan, find_route, link_plan};

/// The register bus of a bridged sensor.
pub type Bus = I2cRegisterBus<I2cDevice>;
/// The pins of a bridged sensor (the bridge's power control).
pub type Pins = BridgePins<Arc<SensorBridge>>;
/// Typed, frame-accurate controls of an open camera.
pub type CameraControls = ControlHandle<Bus, Pins>;

const CAPTURE: BufType = BufType::VideoCapture;
/// `V4L2_COLORSPACE_RAW`.
const COLORSPACE_RAW: u32 = 11;
/// `V4L2_FIELD_NONE`.
const FIELD_NONE: u32 = 1;

/// How a camera is opened.
#[derive(Clone, Debug)]
pub struct CameraOptions {
    /// Capture buffers (at least 2).
    pub buffers: u32,
    /// Where buffers come from.
    pub memory: BufferMemory,
    /// How long the bridge waits for a start/stop acknowledgement.
    pub ack_timeout: Duration,
    /// Wait after switching the bridge's power on (regulators without a start-up delay).
    pub power_settle: Duration,
    /// I²C adapter, when the bridge does not name one.
    pub i2c_bus: Option<u32>,
    /// Use the receiver's frame-start events (else frame starts are inferred from dequeues).
    pub frame_sync: bool,
    /// Capture the sensor's embedded data when the bridge and the description provide it, and
    /// report the values it carries for each frame.
    pub embedded_data: bool,
}

impl Default for CameraOptions {
    fn default() -> Self {
        Self {
            buffers: 4,
            memory: BufferMemory::Mmap,
            ack_timeout: Duration::from_millis(1000),
            power_settle: Duration::from_millis(5),
            i2c_bus: None,
            frame_sync: true,
            embedded_data: true,
        }
    }
}

/// What to capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSettings {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Pixel format; `None` picks the receiver's packed raw format.
    pub fourcc: Option<FourCc>,
    /// Media bus code (bit depth); `None` takes the first mode of that size (or the one the
    /// pixel format implies).
    pub code: Option<u32>,
    /// Frame interval; `None` keeps the mode's default.
    pub interval: Option<Fraction>,
}

impl StreamSettings {
    /// A size at the default rate and format.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            fourcc: None,
            code: None,
            interval: None,
        }
    }

    /// At this frame rate.
    pub fn fps(mut self, fps: u32) -> Self {
        self.interval = Some(Fraction::from_fps(fps.max(1)));
        self
    }

    /// At this frame interval.
    pub fn interval(mut self, interval: Fraction) -> Self {
        self.interval = Some(interval);
        self
    }

    /// In this pixel format.
    pub fn fourcc(mut self, fourcc: FourCc) -> Self {
        self.fourcc = Some(fourcc);
        self
    }
}

/// The configuration in effect.
#[derive(Clone, Debug, PartialEq)]
pub struct Configured {
    /// The sensor mode.
    pub mode: SensorMode,
    /// Pixel format in memory.
    pub fourcc: FourCc,
    /// Bytes per line.
    pub stride: u32,
    /// Bytes per frame.
    pub size_image: u32,
    /// The frame interval the sensor is set to (exact).
    pub interval: Fraction,
    /// Frame length in lines.
    pub frame_length: u32,
}

/// Picks the mode for the settings.
pub fn select_mode<'a>(
    modes: &'a [SensorMode],
    s: &StreamSettings,
    offered: &[(u32, Vec<FourCc>)],
) -> Result<&'a SensorMode> {
    let fits = |m: &&SensorMode| {
        m.width == s.width
            && m.height == s.height
            && s.code.is_none_or(|c| c == m.code)
            && s.fourcc.is_none_or(|f| {
                crate::graph::raw_formats_for(m.code, offered)
                    .iter()
                    .any(|g| g.0 == f.0)
            })
    };
    let mode = modes.iter().find(fits).ok_or_else(|| {
        NativeError::InvalidConfig(format!(
            "no mode {}x{}{}",
            s.width,
            s.height,
            s.fourcc.map_or(String::new(), |f| format!(" in {f}"))
        ))
    })?;
    if let Some(i) = s.interval
        && !mode.allows(i)
    {
        return Err(NativeError::InvalidConfig(format!(
            "{:.3} fps is outside {} {}'s {:.3}..{:.3} fps",
            i.fps(),
            mode.mode,
            mode.format,
            mode.min_fps(),
            mode.max_fps()
        )));
    }
    Ok(mode)
}

pub(crate) struct Running {
    /// The raw node's stream; `None` when another device owns the capture nodes.
    pub(crate) shared: Option<Arc<StreamShared>>,
    pub(crate) events: EventThread,
}

/// An open camera. Exclusive: a second open of the same bridge fails with
/// [`NativeError::Busy`].
pub struct NativeCamera {
    pub(crate) info: CameraInfo,
    pub(crate) options: CameraOptions,
    pub(crate) bridge: Arc<SensorBridge>,
    pub(crate) control: Arc<Mutex<SensorControl<Bus, Pins>>>,
    pub(crate) video: Option<Arc<VideoDevice>>,
    pub(crate) video_fd: Option<Arc<AsyncFd<Arc<VideoDevice>>>>,
    pub(crate) embedded: Option<Arc<EmbeddedCapture>>,
    pub(crate) frame_sync: bool,
    pub(crate) configured: Option<Configured>,
    pub(crate) running: Option<Running>,
    opened: Instant,
    _lock: File,
}

impl NativeCamera {
    /// Opens the bridge and the sensor's I²C address and builds the driver. Nothing is
    /// powered until [`Self::configure`].
    pub fn open(info: CameraInfo, options: CameraOptions) -> Result<Self> {
        let opened = Instant::now();
        let loc = &info.location;
        let lock = File::open(&loc.subdev).step("open bridge")?;
        if lock.try_lock().is_err() {
            return Err(NativeError::Busy(format!(
                "{} is in use by another camera",
                loc.subdev.display()
            )));
        }
        let bridge = Arc::new(SensorBridge::open(&loc.subdev).step("open bridge")?);
        let desc = Arc::clone(&info.description);
        let bus = options
            .i2c_bus
            .or(loc.i2c_bus)
            .ok_or_else(|| NativeError::InvalidConfig("the bridge names no I2C bus".into()))?;
        let addr = loc
            .i2c_address
            .or(desc.sensor.i2c_address)
            .ok_or_else(|| NativeError::InvalidConfig("no I2C address".into()))?;
        let width = if desc.sensor.address_bits == 8 {
            AddrWidth::Bits8
        } else {
            AddrWidth::Bits16
        };
        let dev = I2cDevice::open(bus, addr, width).step(&format!("claim I2C {bus}-{addr:04x}"))?;
        let regbus = I2cRegisterBus::new(dev, desc.sensor.address_bits).step("register bus")?;
        let supplies: Vec<&str> = [&desc.sequences.power_up, &desc.sequences.power_down]
            .into_iter()
            .flatten()
            .filter_map(|s| match s {
                Step::Supply { role, .. } => Some(role.as_str()),
                _ => None,
            })
            .collect();
        let rate = u32::try_from(loc.clock_frequency).unwrap_or(0);
        let clocks: Vec<(&str, u32)> = desc
            .sensor
            .clocks
            .keys()
            .map(|c| (c.as_str(), rate))
            .collect();
        let pins = BridgePins::new(
            Arc::clone(&bridge),
            &supplies,
            &clocks,
            options.power_settle,
        );
        // Subscribed for the whole time the camera is open: the bridge refuses to start
        // without a subscriber.
        bridge.subscribe().step("subscribe to bridge events")?;
        let driver = SensorDriver::new(desc, regbus, pins);
        Ok(Self {
            info,
            options,
            bridge,
            control: Arc::new(Mutex::new(SensorControl::new(driver))),
            video: None,
            video_fd: None,
            embedded: None,
            frame_sync: false,
            configured: None,
            running: None,
            opened,
            _lock: lock,
        })
    }

    /// What discovery found (the graph follows the link changes made here).
    pub fn info(&self) -> &CameraInfo {
        &self.info
    }

    /// The configuration in effect.
    pub fn configured(&self) -> Option<&Configured> {
        self.configured.as_ref()
    }

    /// When the camera was opened.
    pub fn opened(&self) -> Instant {
        self.opened
    }

    /// Whether frames are streaming.
    pub fn is_streaming(&self) -> bool {
        self.running.is_some()
    }

    /// Typed, frame-accurate controls (valid while the camera is open).
    pub fn controls(&self) -> CameraControls {
        let bridge = Arc::clone(&self.bridge);
        let hook: BlankingHook = Arc::new(move |h, v| {
            let _ = bridge.set_blanking(h as i32, v as i32);
        });
        ControlHandle::new(Arc::clone(&self.control), Some(hook))
    }

    /// Powers and configures the sensor, the bridge and the receiver path for `settings`.
    /// Fails with [`NativeError::Busy`] while streaming.
    pub fn configure(&mut self, settings: &StreamSettings) -> Result<Configured> {
        let (mode, frame_length) = self.configure_sensor(settings)?;
        self.configure_receiver(&mode)?;
        let (fourcc, stride, size_image) = self.configure_video(&mode, settings.fourcc)?;
        let configured = Configured {
            interval: crate::modes::frame_interval(&mode.timing, frame_length),
            mode,
            fourcc,
            stride,
            size_image,
            frame_length,
        };
        self.configured = Some(configured.clone());
        Ok(configured)
    }

    /// The sensor and bridge part of [`Self::configure`]: power, mode, initial frame duration,
    /// bridge format and timing. Returns the mode and the frame length.
    pub(crate) fn configure_sensor(
        &mut self,
        settings: &StreamSettings,
    ) -> Result<(SensorMode, u32)> {
        if self.running.is_some() {
            return Err(NativeError::Busy(
                "stop streaming before configuring".into(),
            ));
        }
        let mode = select_mode(&self.info.modes, settings, &self.info.raw_formats)?.clone();
        let format = self
            .info
            .description
            .formats
            .get(&mode.format)
            .cloned()
            .ok_or(NativeError::State("mode format vanished"))?;
        let frame_length = {
            let mut c = lock(&self.control);
            c.bring_up(&mode.mode, &mode.format)?;
            let t = c.timing().ok_or(NativeError::State("no mode"))?;
            match settings.interval {
                Some(i) => {
                    let d = interval_duration(i);
                    c.request_at(
                        0,
                        &ControlRequest {
                            frame_duration: Some(d),
                            ..Default::default()
                        },
                    )?;
                    t.frame_length_for_duration(d).lines
                }
                None => t.frame_length_default(),
            }
        };
        let t = mode.timing;
        let freqs = self.bridge.link_frequencies().to_vec();
        let index = match format.link_frequency {
            Some(f) => freqs.iter().position(|&x| x == f as i64).ok_or_else(|| {
                NativeError::InvalidConfig(format!(
                    "link frequency {f} not in the bridge's {freqs:?}"
                ))
            })?,
            None => 0,
        };
        let pad = PadFormat {
            code: mode.code,
            width: mode.width,
            height: mode.height,
        };
        let got = self.bridge.set_format(pad).step("bridge format")?;
        if got != pad {
            return Err(NativeError::InvalidConfig(format!(
                "bridge chose {got:?} for {pad:?}"
            )));
        }
        self.bridge
            .set_timing(BridgeTiming {
                link_freq_index: index as u32,
                pixel_rate: t.pixel_rate as i64,
                hblank: t.hblank as i32,
                vblank: (frame_length - t.height) as i32,
            })
            .step("bridge timing")?;
        self.bridge
            .set_ack_timeout(self.options.ack_timeout)
            .step("bridge ack timeout")?;
        lock(&self.control).expect_start(ExpectedStart {
            code: mode.code,
            width: mode.width,
            height: mode.height,
            link_freq: freqs.get(index).copied().unwrap_or(0),
        });
        Ok((mode, frame_length))
    }

    fn configure_receiver(&mut self, mode: &SensorMode) -> Result<()> {
        let media = MediaDevice::open(&self.info.media).step("open media device")?;
        let mut topo = media.topology().step("media topology")?;
        let (_, _, entity) = find_media(&self.info.location)?;
        let route = find_route(&topo, entity)?;
        self.embedded = None;
        let mut plan = link_plan(&topo, &route);
        let embedded = (self.options.embedded_data
            && self.info.description.embedded_data.is_some())
        .then(|| embedded_link(&topo, &route))
        .flatten();
        plan.extend(embedded);
        for c in &plan {
            let flags = if c.enable {
                LinkFlags::ENABLED
            } else {
                LinkFlags::empty()
            };
            media
                .setup_link(c.source, c.sink, flags)
                .step("MEDIA_IOC_SETUP_LINK")?;
        }
        apply_plan(&mut topo, &plan);
        if embedded.is_some()
            && let Some(path) = route.embedded_node.and_then(|n| topo.devnode_path(n))
        {
            let sensor: Arc<dyn SensorSide> = self.control.clone();
            self.embedded = Some(Arc::new(EmbeddedCapture::open(&path, 4, sensor)?));
        }
        self.info.route = route.clone();
        self.info.rebuild_graph(topo);
        let path = route
            .receiver_path
            .as_ref()
            .ok_or_else(|| NativeError::Topology("the receiver has no subdev node".into()))?;
        let csi = Subdev::open(path).step("open receiver")?;
        let fmt = MbusFormat {
            width: mode.width,
            height: mode.height,
            code: MbusCode(mode.code),
            field: FIELD_NONE,
            colorspace: COLORSPACE_RAW,
            ..Default::default()
        };
        let sink = csi
            .set_format(route.receiver_sink, Which::Active, &fmt)
            .step("receiver sink format")?;
        let source = csi
            .format(route.receiver_source, Which::Active)
            .step("receiver source format")?;
        for (pad, f) in [(route.receiver_sink, sink), (route.receiver_source, source)] {
            if (f.width, f.height, f.code) != (fmt.width, fmt.height, fmt.code) {
                return Err(NativeError::Topology(format!(
                    "receiver pad {pad} has {f:?}"
                )));
            }
        }
        Ok(())
    }

    fn configure_video(
        &mut self,
        mode: &SensorMode,
        want: Option<FourCc>,
    ) -> Result<(FourCc, u32, u32)> {
        if self.video.is_none() {
            let path =
                self.info.route.node_path.clone().ok_or_else(|| {
                    NativeError::Topology("the raw node has no device node".into())
                })?;
            let video = Arc::new(VideoDevice::open(&path).step("open video node")?);
            if !video.capabilities().buffer_types().contains(&CAPTURE) {
                return Err(NativeError::InvalidConfig(format!(
                    "{} is not a single-planar capture node",
                    path.display()
                )));
            }
            let fd = AsyncFd::new(Arc::clone(&video)).step("register video node")?;
            self.frame_sync = self.options.frame_sync
                && video
                    .subscribe(EventType::FrameSync, 0, SubscribeFlags::empty())
                    .is_ok();
            self.video_fd = Some(Arc::new(fd));
            self.video = Some(video);
        }
        let video = self.video.as_ref().expect("opened above");
        let offered: Vec<FourCc> = video
            .formats_for_mbus_code(CAPTURE, mode.code)
            .step("enumerate formats")?
            .iter()
            .map(|f| f.fourcc)
            .collect();
        let fourcc = match want {
            Some(f) if offered.is_empty() || offered.contains(&f) => f,
            Some(f) => {
                return Err(NativeError::InvalidConfig(format!(
                    "{} does not write {f} for this mode (offers {offered:?})",
                    video.path().display()
                )));
            }
            None => formats::choose(mode.code, &offered).ok_or_else(|| {
                NativeError::InvalidConfig("the raw node offers no format".into())
            })?,
        };
        let set = video
            .set_format(
                CAPTURE,
                &Format::Single(PixFormat {
                    width: mode.width,
                    height: mode.height,
                    fourcc,
                    field: FIELD_NONE,
                    ..Default::default()
                }),
            )
            .step("VIDIOC_S_FMT")?;
        let Format::Single(p) = set else {
            return Err(NativeError::State("unexpected format type"));
        };
        if (p.width, p.height, p.fourcc) != (mode.width, mode.height, fourcc) {
            return Err(NativeError::InvalidConfig(format!(
                "video node chose {p:?}"
            )));
        }
        Ok((fourcc, p.bytes_per_line, p.size_image))
    }

    /// Starts streaming: returns once the sensor streams (the bridge's start request was
    /// served).
    pub fn start(&mut self) -> Result<FrameStream> {
        if self.running.is_some() {
            return Err(NativeError::Busy("already streaming".into()));
        }
        let cfg = self
            .configured
            .clone()
            .ok_or(NativeError::State("configure before starting"))?;
        let video = Arc::clone(self.video.as_ref().expect("configured"));
        let fd = Arc::clone(self.video_fd.as_ref().expect("configured"));
        let sensor: Arc<dyn SensorSide> = self.control.clone();
        let events = EventThread::spawn(
            Arc::clone(&self.bridge),
            self.frame_sync.then(|| Arc::clone(&fd)),
            Arc::clone(&sensor),
            self.embedded.clone(),
        )
        .step("start the event thread")?;
        let buffers = BufferSet::allocate(
            Arc::clone(&video),
            CAPTURE,
            &self.options.memory,
            self.options.buffers,
            cfg.size_image as usize,
        )?;
        buffers.queue_all()?;
        let lender = Arc::new(Lender {
            buffers: Arc::new(buffers),
            streaming: Arc::new(AtomicBool::new(true)),
        });
        let shared = Arc::new(StreamShared {
            fd,
            lender: Arc::clone(&lender),
            sensor,
            embedded: self.embedded.clone(),
            buf_type: CAPTURE,
            frame_sync: self.frame_sync,
            fourcc: cfg.fourcc,
            width: cfg.mode.width,
            height: cfg.mode.height,
            stride: cfg.stride,
            started: Instant::now(),
            first_frame: OnceLock::new(),
            frames: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_sequence: AtomicU64::new(0),
            disconnected: AtomicBool::new(false),
        });
        if let Some(e) = &self.embedded
            && let Err(err) = e.start()
        {
            lender.streaming.store(false, Ordering::Release);
            events.join();
            return Err(err);
        }
        if let Err(e) = video.stream_on(CAPTURE) {
            if let Some(emb) = &self.embedded {
                emb.stop();
            }
            lender.streaming.store(false, Ordering::Release);
            events.join();
            self.standby();
            return Err(NativeError::kernel("VIDIOC_STREAMON", e));
        }
        self.running = Some(Running {
            shared: Some(Arc::clone(&shared)),
            events,
        });
        Ok(FrameStream::new(shared))
    }

    /// Puts the sensor back in standby if a failed or interrupted start left it streaming.
    pub(crate) fn standby(&self) {
        let mut c = lock(&self.control);
        if c.driver().state() == DriverState::Streaming {
            let _ = c.driver_mut().stop_streaming();
        }
    }

    /// Stops streaming. Frame streams end; frames still held keep their buffers until dropped.
    pub fn stop(&mut self) -> Result<()> {
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        let result = match &running.shared {
            Some(shared) => {
                shared.lender.streaming.store(false, Ordering::Release);
                self.video
                    .as_ref()
                    .map_or(Ok(()), |v| v.stream_off(CAPTURE))
                    .step("VIDIOC_STREAMOFF")
            }
            None => Ok(()),
        };
        running.events.join();
        if let Some(e) = &self.embedded {
            e.stop();
        }
        self.standby();
        result
    }

    /// Frame starts and acknowledgements seen by the event thread of the running stream.
    pub fn event_counts(&self) -> Option<(u64, u64)> {
        self.running.as_ref().map(|r| {
            (
                r.events.stats.frame_syncs.load(Ordering::Relaxed),
                r.events.stats.acks.load(Ordering::Relaxed),
            )
        })
    }

    /// Embedded data buffers reported to the control schedule (0 without embedded data).
    pub fn embedded_reports(&self) -> Option<u64> {
        self.embedded
            .as_ref()
            .map(|e| e.reported.load(Ordering::Relaxed))
    }

    /// Whether frame starts come from `FRAME_SYNC` events.
    pub fn uses_frame_sync(&self) -> bool {
        self.frame_sync
    }

    /// Stops, puts the sensor in standby and powers it down.
    pub fn close(mut self) -> Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<()> {
        let stopped = self.stop();
        let down = lock(&self.control).shut_down();
        if matches!(self.bridge.power(), Ok(true)) {
            let _ = self.bridge.set_power(false);
        }
        stopped.and(down)
    }
}

impl Drop for NativeCamera {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl std::fmt::Debug for NativeCamera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeCamera")
            .field("key", &self.info.key)
            .field("configured", &self.configured)
            .field("streaming", &self.running.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use styx_sensor::SensorDescription;

    use super::*;
    use crate::modes::sensor_modes;

    #[test]
    fn modes_are_selected_by_size_format_and_rate() {
        let desc = SensorDescription::from_file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sensor/sensors/ov9782.toml"
        ))
        .unwrap();
        let modes = sensor_modes(&desc);
        let offered = [(0x3007, vec![FourCc::new(b"pBAA")])];
        let m = select_mode(&modes, &StreamSettings::new(1280, 800).fps(120), &offered).unwrap();
        assert_eq!((m.mode.as_str(), m.format.as_str()), ("1280x800", "raw10"));
        let s = StreamSettings::new(1280, 800).fourcc(FourCc::new(b"BA81"));
        assert_eq!(select_mode(&modes, &s, &offered).unwrap().format, "raw8");
        // The node offers only pBAA for raw10.
        let s = StreamSettings::new(1280, 800).fourcc(FourCc::new(b"BG10"));
        assert!(select_mode(&modes, &s, &offered).is_err());
        let s = StreamSettings::new(1280, 800).fps(121);
        assert!(
            select_mode(&modes, &s, &offered)
                .unwrap_err()
                .to_string()
                .contains("outside")
        );
        assert!(select_mode(&modes, &StreamSettings::new(1920, 1080), &offered).is_err());
        let exact = StreamSettings::new(1280, 800).interval(Fraction::new(8281, 1_000_000));
        assert!(select_mode(&modes, &exact, &offered).is_ok());
        assert_eq!(CameraOptions::default().buffers, 4);
    }
}
