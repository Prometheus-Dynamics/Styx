//! The device side: bridge, sensor driver, media graph, buffers, streaming, the
//! acknowledgement thread, and cleanup on every exit path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use styx_kernel::bus::i2c::{AddrWidth, I2cDevice};
use styx_kernel::bus::{BridgeLocation, PadFormat, SensorBridge, StreamAction, StreamRequest};
use styx_kernel::event::{EventKind, EventType, Events, SubscribeFlags};
use styx_kernel::media::{self, LinkFlags, MediaDevice};
use styx_kernel::subdev::{MbusCode, MbusFormat, Subdev, Which};
use styx_kernel::v4l2::{BufType, Format, Memory, PixFormat, QueueBuffer, VideoDevice};
use styx_kernel::{FourCc, Mapping};
use styx_sensor::{DriverState, SensorDescription, SensorDriver, Step, Timing};

use crate::frames::{self, FrameSample, Raw10Layout};
use crate::pipeline;
use crate::regbus::{BridgePins, I2cRegisterBus};
use crate::{Result, ResultExt, interrupted, log};

/// The sensor driver as the spike runs it.
pub type Driver = SensorDriver<I2cRegisterBus<I2cDevice>, BridgePins<Arc<SensorBridge>>>;

const CAPTURE: BufType = BufType::VideoCapture;
/// `V4L2_COLORSPACE_RAW`.
const COLORSPACE_RAW: u32 = 11;
/// `V4L2_FIELD_NONE`.
const FIELD_NONE: u32 = 1;

fn lock(driver: &Mutex<Driver>) -> MutexGuard<'_, Driver> {
    driver.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the start event must carry for the configuration this spike set up.
#[derive(Debug, Clone, Copy)]
struct Expected {
    code: u32,
    width: u32,
    height: u32,
    link_freq: i64,
}

fn check_request(req: &StreamRequest, want: &Expected) -> std::result::Result<(), String> {
    let got = (req.code, req.width, req.height, req.link_freq);
    let exp = (want.code, want.width, want.height, want.link_freq);
    if got == exp {
        Ok(())
    } else {
        Err(format!("start event carries {got:?}, configured {exp:?}"))
    }
}

/// Serves the bridge's start/stop requests with the sensor driver.
struct AckThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl AckThread {
    fn spawn(bridge: Arc<SensorBridge>, driver: Arc<Mutex<Driver>>, want: Expected) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let keep = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            bridge.serve(
                Duration::from_millis(50),
                || !keep.load(Ordering::Relaxed),
                |req| {
                    let t = Instant::now();
                    let result = serve_one(req, &driver, &want);
                    log!(
                        "[ack] {:?} seq {} ({} ms timeout, vblank {}) -> {} in {:.2} ms",
                        req.action,
                        req.sequence,
                        req.timeout.as_millis(),
                        req.vblank,
                        match &result {
                            Ok(()) => "ok".to_owned(),
                            Err(e) => format!("errno {e}"),
                        },
                        t.elapsed().as_secs_f64() * 1e3
                    );
                    result
                },
            )
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn join(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log!("[ack] thread ended with {e}"),
                Err(_) => log!("[ack] thread panicked"),
            }
        }
    }
}

fn serve_one(
    req: &StreamRequest,
    driver: &Mutex<Driver>,
    want: &Expected,
) -> std::result::Result<(), i32> {
    let mut d = lock(driver);
    match req.action {
        StreamAction::Start => {
            if let Err(e) = check_request(req, want) {
                log!("[ack] refusing start: {e}");
                return Err(libc::EINVAL);
            }
            d.start_streaming().map_err(|e| {
                log!("[ack] start_streaming: {e}");
                libc::EIO
            })
        }
        StreamAction::Stop => {
            if d.state() != DriverState::Streaming {
                return Ok(());
            }
            d.stop_streaming().map_err(|e| {
                log!("[ack] stop_streaming: {e}");
                libc::EIO
            })
        }
    }
}

/// One dequeued frame, optionally with a copy of its bytes.
pub struct Frame {
    /// Summary.
    pub sample: FrameSample,
    /// The bytes, when asked for.
    pub data: Option<Vec<u8>>,
}

/// Everything set up on the device. Dropping it stops streaming, stops the acknowledgement
/// thread, frees the buffers, puts the sensor in standby and switches its power off.
pub struct Rig {
    /// The bridge (shared with the pins and the acknowledgement thread).
    pub bridge: Arc<SensorBridge>,
    /// The sensor driver (shared with the acknowledgement thread).
    pub driver: Arc<Mutex<Driver>>,
    /// Timing of the active mode.
    pub timing: Timing,
    /// Frame layout in memory.
    pub layout: Raw10Layout,
    /// The pixel format of the raw node.
    pub fourcc: FourCc,
    /// Black level at 10 bits.
    pub black: f64,
    /// Line step for mean levels.
    pub row_step: usize,
    /// Capture embedded data too (set before `configure_graph`).
    pub want_embedded: bool,
    /// The embedded data node, when captured.
    pub embedded: Option<crate::embedded::EmbeddedNode>,
    video: Option<VideoDevice>,
    maps: Vec<Mapping>,
    buffers: bool,
    streaming: bool,
    frame_sync: bool,
    ack: Option<AckThread>,
    expected: Option<Expected>,
    last_fs: Option<u32>,
}

impl Rig {
    /// Finds and opens the bridge and the sensor's I²C address, and builds the driver. Nothing
    /// is powered yet.
    pub fn open(
        desc: Arc<SensorDescription>,
        loc: &BridgeLocation,
        i2c_bus: Option<u32>,
        settle: Duration,
        row_step: usize,
        mode: &str,
        format: &str,
    ) -> Result<Self> {
        let bridge = Arc::new(SensorBridge::open(&loc.subdev).ctx("open bridge")?);
        let bus = i2c_bus
            .or(loc.i2c_bus)
            .ok_or("the bridge names no I2C bus; pass --i2c-bus")?;
        let addr = loc
            .i2c_address
            .or(desc.sensor.i2c_address)
            .ok_or("no I2C address in the bridge or the description")?;
        let width = if desc.sensor.address_bits == 8 {
            AddrWidth::Bits8
        } else {
            AddrWidth::Bits16
        };
        let dev = I2cDevice::open(bus, addr, width).ctx(&format!(
            "claim I2C {bus}-{addr:04x} (is a kernel driver still bound?)"
        ))?;
        log!(
            "I2C: /dev/i2c-{bus} address {addr:#04x}, {}-bit registers",
            desc.sensor.address_bits
        );
        let regbus = I2cRegisterBus::new(dev, desc.sensor.address_bits).ctx("register bus")?;
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
        let pins = BridgePins::new(Arc::clone(&bridge), &supplies, &clocks, settle);
        let m = desc.mode(mode).ctx("mode")?;
        let f = desc.format_for(m, format).ctx("format")?;
        let timing = desc.timing(mode, format).ctx("timing")?;
        let black = desc.pixel_array.black_level.map_or(0.0, |b| b.at_bits(10));
        let layout = Raw10Layout {
            width: m.size.width as usize,
            height: m.size.height as usize,
            stride: m.size.width as usize * 5 / 4,
        };
        if f.bits() != 10 {
            return Err(format!(
                "format {format} is {}-bit; the spike handles raw10",
                f.bits()
            ));
        }
        Ok(Self {
            driver: Arc::new(Mutex::new(SensorDriver::new(desc, regbus, pins))),
            bridge,
            timing,
            layout,
            fourcc: FourCc::default(),
            black,
            row_step,
            want_embedded: false,
            embedded: None,
            video: None,
            maps: Vec::new(),
            buffers: false,
            streaming: false,
            frame_sync: false,
            ack: None,
            expected: None,
            last_fs: None,
        })
    }

    /// Locks the driver.
    pub fn driver(&self) -> MutexGuard<'_, Driver> {
        lock(&self.driver)
    }

    /// Subscribes to the bridge's stream events, powers the sensor, checks its chip id (burst
    /// and byte by byte), writes init and the mode. The sensor stays in software standby.
    pub fn bring_up(&mut self, mode: &str, format: &str) -> Result<()> {
        self.bridge.subscribe().ctx("subscribe to bridge events")?;
        let mut d = self.driver();
        d.power_up().ctx("power up")?;
        log!(
            "power: on (bridge power control reads {:?})",
            self.bridge.power()
        );
        let id = d.verify_chip_id().ctx("chip id")?;
        if let Some(c) = d.description().sensor.chip_id.clone() {
            let bytewise = d
                .bus_mut()
                .read_bytewise(c.address, c.bytes)
                .ctx("chip id bytes")?;
            log!("chip id {id:#06x} (burst read), {bytewise:#06x} (byte reads)");
            if bytewise != id {
                return Err(format!("chip id burst {id:#x} != bytewise {bytewise:#x}"));
            }
        }
        d.init().ctx("init")?;
        let active = d.set_mode(mode, format).ctx("set mode")?;
        log!(
            "mode {} {} code {:#06x}: line length {}, fps {:.2}..{:.2}; sensor in standby",
            active.mode,
            active.format,
            active.code.0,
            active.timing.line_length(),
            active.timing.fps_range().0,
            active.timing.fps_range().1
        );
        Ok(())
    }

    /// Sets the bridge's pad format and timing controls, and the acknowledgement timeout.
    pub fn configure_bridge(
        &mut self,
        link_frequency: Option<u64>,
        ack_timeout: Duration,
    ) -> Result<()> {
        let code = self.driver().mode().ok_or("no mode")?.code.0;
        let t = self.timing;
        let want = PadFormat {
            code,
            width: t.width,
            height: t.height,
        };
        let got = self.bridge.set_format(want).ctx("bridge format")?;
        if got != want {
            return Err(format!("bridge chose {got:?} for {want:?}"));
        }
        let freqs = self.bridge.link_frequencies().to_vec();
        let index = match link_frequency {
            Some(f) => freqs
                .iter()
                .position(|&x| x == f as i64)
                .ok_or_else(|| format!("link frequency {f} not in the bridge's {freqs:?}"))?,
            None => 0,
        };
        let vblank = t.frame_length_default() - t.height;
        self.bridge
            .set_timing(styx_kernel::bus::Timing {
                link_freq_index: index as u32,
                pixel_rate: t.pixel_rate as i64,
                hblank: t.hblank as i32,
                vblank: vblank as i32,
            })
            .ctx("bridge timing")?;
        self.bridge
            .set_ack_timeout(ack_timeout)
            .ctx("ack timeout")?;
        self.expected = Some(Expected {
            code,
            width: t.width,
            height: t.height,
            link_freq: freqs[index],
        });
        log!(
            "bridge {}: {}x{} code {code:#06x}, link {} Hz, pixel rate {}, hblank {}, vblank {vblank}",
            self.bridge.path().display(),
            t.width,
            t.height,
            freqs[index],
            t.pixel_rate,
            t.hblank
        );
        Ok(())
    }

    /// Enables the raw path in the media graph, sets the receiver pads and the video format.
    pub fn configure_graph(&mut self) -> Result<()> {
        let (media, topo, path) = find_media(self.bridge.path())?;
        for c in pipeline::link_plan(&topo, &path) {
            log!("media: {}", pipeline::describe(&topo, &c));
            let flags = if c.enable {
                LinkFlags::ENABLED
            } else {
                LinkFlags::empty()
            };
            media
                .setup_link(c.source, c.sink, flags)
                .ctx("setup link")?;
        }
        if self.want_embedded {
            let node = crate::embedded::enable_link(&media, &topo, path.receiver)?;
            self.embedded = Some(crate::embedded::EmbeddedNode::open(&node, 4)?);
        }
        let e = self.expected.ok_or("configure the bridge first")?;
        let csi = Subdev::open(
            path.receiver_path
                .as_ref()
                .ok_or("no receiver subdev node")?,
        )
        .ctx("open receiver")?;
        let fmt = MbusFormat {
            width: e.width,
            height: e.height,
            code: MbusCode(e.code),
            field: FIELD_NONE,
            colorspace: COLORSPACE_RAW,
            ..Default::default()
        };
        let sink = csi
            .set_format(path.receiver_sink, Which::Active, &fmt)
            .ctx("receiver sink format")?;
        let source = csi
            .format(path.receiver_source, Which::Active)
            .ctx("receiver source format")?;
        for (pad, f) in [(path.receiver_sink, sink), (path.receiver_source, source)] {
            if (f.width, f.height, f.code) != (fmt.width, fmt.height, fmt.code) {
                return Err(format!("receiver pad {pad} has {f:?}"));
            }
        }
        log!(
            "receiver pads {} and {}: {}x{} {:?}",
            path.receiver_sink,
            path.receiver_source,
            e.width,
            e.height,
            fmt.code
        );
        let video = VideoDevice::open(path.node_path.as_ref().ok_or("no raw video node")?)
            .ctx("open video node")?;
        let offered: Vec<FourCc> = video
            .formats_for_mbus_code(CAPTURE, e.code)
            .ctx("enumerate formats")?
            .iter()
            .map(|f| f.fourcc)
            .collect();
        let fourcc = frames::choose_format(&offered)
            .ok_or("the raw node offers no format for the bus code")?;
        let set = video
            .set_format(
                CAPTURE,
                &Format::Single(PixFormat {
                    width: e.width,
                    height: e.height,
                    fourcc,
                    field: FIELD_NONE,
                    ..Default::default()
                }),
            )
            .ctx("video format")?;
        let Format::Single(p) = set else {
            return Err(format!("unexpected format {set:?}"));
        };
        if (p.width, p.height, p.fourcc) != (e.width, e.height, fourcc) {
            return Err(format!("video node chose {p:?}"));
        }
        self.layout.stride = p.bytes_per_line as usize;
        self.fourcc = fourcc;
        log!(
            "video {}: {fourcc} {}x{} stride {} size {} (offered {offered:?})",
            video.path().display(),
            p.width,
            p.height,
            p.bytes_per_line,
            p.size_image
        );
        self.video = Some(video);
        Ok(())
    }

    fn video(&self) -> Result<&VideoDevice> {
        self.video
            .as_ref()
            .ok_or_else(|| "no video node".to_owned())
    }

    /// Starts the acknowledgement thread, allocates, maps and queues buffers, subscribes to
    /// frame-start events and starts streaming (which returns once the sensor started).
    pub fn start(&mut self, buffers: u32) -> Result<()> {
        let want = self.expected.ok_or("configure the bridge first")?;
        self.ack = Some(AckThread::spawn(
            Arc::clone(&self.bridge),
            Arc::clone(&self.driver),
            want,
        ));
        let video = self.video.take().ok_or("no video node")?;
        let res = self.start_with(&video, buffers);
        self.video = Some(video);
        res
    }

    fn start_with(&mut self, video: &VideoDevice, buffers: u32) -> Result<()> {
        let got = video
            .request_buffers(CAPTURE, Memory::Mmap, buffers)
            .ctx("REQBUFS")?;
        self.buffers = true;
        for i in 0..got.count {
            let mut planes = video.map_buffer(CAPTURE, i).ctx("mmap buffer")?;
            self.maps.push(planes.remove(0));
            video.queue(&QueueBuffer::mmap(CAPTURE, i)).ctx("QBUF")?;
        }
        self.frame_sync = match video.subscribe(EventType::FrameSync, 0, SubscribeFlags::empty()) {
            Ok(()) => true,
            Err(e) => {
                log!("frame-start events unavailable ({e}); using dequeue time");
                false
            }
        };
        log!(
            "{} buffers of {} bytes queued; STREAMON",
            got.count,
            self.maps[0].len()
        );
        if let Some(emb) = &mut self.embedded {
            emb.start()?;
        }
        let t = Instant::now();
        video.stream_on(CAPTURE).ctx("STREAMON")?;
        self.streaming = true;
        log!(
            "streaming (STREAMON took {:.1} ms, bridge state {:?})",
            t.elapsed().as_secs_f64() * 1e3,
            self.bridge.stream_state()
        );
        Ok(())
    }

    /// Waits up to `timeout` for the next frame, calling the driver's `frame_start` for every
    /// frame-start event on the way. Returns `None` on timeout.
    pub fn next_frame(&mut self, timeout: Duration, copy: bool) -> Result<Option<Frame>> {
        let deadline = Instant::now() + timeout;
        loop {
            if interrupted() {
                return Err("interrupted".into());
            }
            let video = self.video()?;
            let left = deadline.saturating_duration_since(Instant::now());
            let ready = video
                .wait(Some(left.min(Duration::from_millis(100))))
                .ctx("poll")?;
            if ready.error {
                return Err("the video node reported an error (streaming stopped?)".into());
            }
            let mut starts = Vec::new();
            while let Some(ev) = video.dequeue_event().ctx("DQEVENT")? {
                if let EventKind::FrameSync { frame_sequence } = ev.kind {
                    starts.push(frame_sequence);
                }
            }
            for s in starts {
                self.frame_start(s);
            }
            let buf = self.video()?.dequeue(CAPTURE, Memory::Mmap).ctx("DQBUF")?;
            if let Some(emb) = &mut self.embedded {
                emb.poll()?;
            }
            if let Some(buf) = buf {
                if !self.frame_sync {
                    // Dequeued after its end: the next frame is starting.
                    self.frame_start(buf.sequence.wrapping_add(1));
                }
                let video = self.video()?;
                let map = &self.maps[buf.index as usize];
                let used = buf.bytes_used().min(map.len());
                let bytes = &map.as_slice()[..used];
                let mean = self
                    .layout
                    .mean_level(bytes, self.row_step)
                    .unwrap_or(f64::NAN);
                let data = copy.then(|| bytes.to_vec());
                let sample = FrameSample {
                    sequence: buf.sequence,
                    timestamp: buf.timestamp,
                    mean,
                    error: buf.flags.contains(styx_kernel::v4l2::BufferFlags::ERROR),
                };
                video
                    .queue(&QueueBuffer::mmap(CAPTURE, buf.index))
                    .ctx("QBUF")?;
                return Ok(Some(Frame { sample, data }));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    fn frame_start(&mut self, seq: u32) {
        if self
            .last_fs
            .is_some_and(|l| seq.wrapping_sub(l) >= u32::MAX / 2 || seq == l)
        {
            return;
        }
        self.last_fs = Some(seq);
        let mut d = self.driver();
        if d.state() != DriverState::Streaming {
            return;
        }
        match d.frame_start(u64::from(seq)) {
            Ok(batch) if !batch.controls.is_empty() => {
                log!("[frame {seq}] wrote {:?}", batch.controls);
            }
            Ok(_) => {}
            Err(e) => log!("[frame {seq}] frame_start: {e}"),
        }
    }

    /// The last frame-start sequence seen.
    pub fn current_frame(&self) -> Option<u32> {
        self.last_fs
    }

    /// Stops streaming and tears everything down, reporting what it did.
    pub fn shutdown(&mut self) {
        if self.streaming {
            self.streaming = false;
            if let Some(v) = &self.video {
                let t = Instant::now();
                match v.stream_off(CAPTURE) {
                    Ok(()) => log!("STREAMOFF ({:.1} ms)", t.elapsed().as_secs_f64() * 1e3),
                    Err(e) => log!("STREAMOFF: {e}"),
                }
            }
        }
        self.embedded = None;
        if let Some(ack) = self.ack.take() {
            ack.join();
        }
        self.maps.clear();
        if self.buffers {
            self.buffers = false;
            if let Some(v) = &self.video
                && let Err(e) = v.free_buffers(CAPTURE, Memory::Mmap)
            {
                log!("freeing buffers: {e}");
            }
        }
        self.video = None;
        {
            let mut d = self.driver();
            if d.state() == DriverState::Streaming {
                log!("sensor still streaming after STREAMOFF: stopping it");
                if let Err(e) = d.stop_streaming() {
                    log!("stop_streaming: {e}");
                }
            }
            if d.state() == DriverState::Powered {
                // Standby again whatever happened (a failed start may have got half way).
                let off: Vec<_> = d
                    .description()
                    .sequences
                    .stream_off
                    .iter()
                    .filter_map(Step::as_write)
                    .copied()
                    .collect();
                if let Err(e) = styx_sensor::RegisterBus::write_sequence(d.bus_mut(), &off) {
                    log!("writing stream_off: {e}");
                }
            }
            if d.state() != DriverState::Off
                && let Err(e) = d.power_down()
            {
                log!("power_down: {e}");
            }
        }
        match self.bridge.power() {
            Ok(true) => {
                if let Err(e) = self.bridge.set_power(false) {
                    log!("switching the bridge off: {e}");
                }
            }
            Ok(false) => {}
            Err(e) => log!("reading bridge power: {e}"),
        }
        log!(
            "cleanup: bridge state {:?}, power {:?}",
            self.bridge.stream_state(),
            self.bridge.power()
        );
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Finds the media device whose raw path starts at the bridge (opened read-write).
fn find_media(
    bridge: &std::path::Path,
) -> Result<(MediaDevice, media::Topology, pipeline::RawPath)> {
    for p in media::list_media_devices() {
        let Ok(dev) = MediaDevice::open(&p) else {
            continue;
        };
        let Ok(topo) = dev.topology() else { continue };
        if let Ok(path) = pipeline::find_raw_path(&topo)
            && path.sensor_path.as_deref() == Some(bridge)
        {
            log!("media {}: sensor entity \"{}\"", p.display(), path.sensor.1);
            return Ok((dev, topo, path));
        }
    }
    Err(format!(
        "no media graph has {} as the sensor of csi2",
        bridge.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_requests_must_match_the_configuration() {
        let want = Expected {
            code: 0x3007,
            width: 1280,
            height: 800,
            link_freq: 400_000_000,
        };
        let mut req = StreamRequest {
            action: StreamAction::Start,
            sequence: 1,
            timeout: Duration::from_secs(1),
            link_freq: 400_000_000,
            pixel_rate: 160_000_000,
            code: 0x3007,
            width: 1280,
            height: 800,
            hblank: 176,
            vblank: 1022,
            data_lanes: 2,
            continuous_clock: true,
        };
        assert!(check_request(&req, &want).is_ok());
        req.width = 640;
        assert!(check_request(&req, &want).is_err());
    }
}
