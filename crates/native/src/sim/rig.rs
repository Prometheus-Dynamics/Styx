//! A simulated camera's devices and frame producer, behind a [`NativeCamera`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::bus::{StreamAction, StreamRequest};
use styx_sensor::{SensorDescription, SensorDriver, Step};

use super::scene::{Exposure, FrameContext, GroundTruth, Readout, SimScene, render};
use super::{SimBus, SimSpec, present, scene_for, spec_of, unplug};
use crate::camera::{
    Configured, NativeCamera, StreamSettings, check_flips, select_mode, start_frame_length,
};
use crate::control::{ExpectedStart, lock};
use crate::device::{BridgeDevice, CaptureDevice};
use crate::discover::CameraInfo;
use crate::error::{NativeError, Result};
use crate::fake::FakeQueue;
use crate::fake_bridge::FakeBridge;
use crate::formats;
use crate::sensor_bus::{BridgePins, CameraPins, SensorBus};
use crate::session::{BufferSource, Session, SessionOptions};
use crate::stream::SensorSide;

/// Simulated cameras open now (each opens once, as a bridge does).
static OPEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A simulated camera's sensor registers, bridge, capture queue and frame producer.
pub(crate) struct SimRig {
    key: String,
    spec: SimSpec,
    scene: Arc<dyn SimScene>,
    desc: Arc<SensorDescription>,
    bus: SimBus,
    pub(crate) bridge: Arc<FakeBridge>,
    /// The capture queue of the configured frame size.
    queue: Mutex<Option<Arc<FakeQueue>>>,
    /// How frames are written, and the part of the array the mode reads.
    readout: Mutex<Option<(Readout, [f32; 4])>>,
    control: Arc<Mutex<crate::control::SensorControl<crate::camera::Bus, crate::camera::Pins>>>,
    stop: AtomicBool,
    /// Where each frame's ground truth goes (`SimSpec::truth`).
    truth: Mutex<Option<std::io::BufWriter<std::fs::File>>>,
}

/// One frame's ground truth as a line of JSON, its points in the delivered frame (mirrored as
/// the flips read the array out).
fn truth_line(
    seq: u32,
    timestamp: Duration,
    (w, h): (u32, u32),
    (hflip, vflip): (bool, bool),
    t: &GroundTruth,
) -> String {
    let (p, q) = t.camera;
    let mut line = format!(
        "{{\"sequence\":{seq},\"timestamp_ns\":{},\"width\":{w},\"height\":{h},\"hflip\":{hflip},\"vflip\":{vflip},\"camera\":{{\"position\":[{:.6},{:.6},{:.6}],\"orientation_wxyz\":[{:.8},{:.8},{:.8},{:.8}]}},\"points\":[",
        timestamp.as_nanos(),
        p[0],
        p[1],
        p[2],
        q[0],
        q[1],
        q[2],
        q[3]
    );
    for (i, (name, [x, y])) in t.points.iter().enumerate() {
        let x = if hflip { f64::from(w) - 1.0 - x } else { *x };
        let y = if vflip { f64::from(h) - 1.0 - y } else { *y };
        let name = name.replace('\\', "\\\\").replace('"', "\\\"");
        line.push_str(&format!(
            "{}{{\"name\":\"{name}\",\"x\":{x:.3},\"y\":{y:.3}}}",
            if i > 0 { "," } else { "" }
        ));
    }
    line.push_str("]}\n");
    line
}

impl Drop for SimRig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        lock(&OPEN).retain(|k| *k != self.key);
    }
}

fn idle_request() -> StreamRequest {
    StreamRequest {
        action: StreamAction::Start,
        sequence: 0,
        timeout: Duration::ZERO,
        link_freq: 0,
        pixel_rate: 0,
        code: 0,
        width: 0,
        height: 0,
        hblank: 0,
        vblank: 0,
        data_lanes: 2,
        continuous_clock: true,
    }
}

/// Opens the simulated camera `info` (its spec from the configuration).
pub(crate) fn open(
    info: CameraInfo,
    options: crate::camera::CameraOptions,
    opened: Instant,
) -> Result<NativeCamera> {
    let spec = spec_of(&info.key)
        .ok_or_else(|| NativeError::Topology(format!("no simulated camera {}", info.key)))?;
    open_spec(info, options, opened, spec)
}

/// Opens the simulated camera `info` as `spec` says.
pub(crate) fn open_spec(
    info: CameraInfo,
    options: crate::camera::CameraOptions,
    opened: Instant,
    spec: SimSpec,
) -> Result<NativeCamera> {
    let key = info.key.clone();
    if !present(&key) {
        return Err(NativeError::Topology(format!("{key} is unplugged")));
    }
    {
        let mut open = lock(&OPEN);
        if open.contains(&key) {
            return Err(NativeError::Busy(format!(
                "{key} is in use by another camera"
            )));
        }
        open.push(key.clone());
    }
    // Released by the rig when it goes; until it exists, by this guard.
    struct Claim(Option<String>);
    impl Drop for Claim {
        fn drop(&mut self) {
            if let Some(k) = self.0.take() {
                lock(&OPEN).retain(|o| *o != k);
            }
        }
    }
    let mut claim = Claim(Some(key.clone()));
    let scene = scene_for(&spec)?;
    let desc = Arc::clone(&info.description);
    let bus = match &desc.sensor.chip_id {
        Some(c) => SimBus::with_chip_id(c.address, c.bytes, c.values.first().copied().unwrap_or(0)),
        None => SimBus::default(),
    };
    let bridge = FakeBridge::new(idle_request());
    let supplies: Vec<&str> = [&desc.sequences.power_up, &desc.sequences.power_down]
        .into_iter()
        .flatten()
        .filter_map(|s| match s {
            Step::Supply { role, .. } => Some(role.as_str()),
            _ => None,
        })
        .collect();
    let clocks: Vec<(&str, u32)> = desc
        .sensor
        .clocks
        .iter()
        .map(|(c, rate)| (c.as_str(), *rate))
        .collect();
    let pins = CameraPins::Sim(BridgePins::new(
        super::SimPower(Arc::clone(&bridge)),
        &supplies,
        &clocks,
        Duration::ZERO,
    ));
    let driver = SensorDriver::new(Arc::clone(&desc), SensorBus::Sim(bus.clone()), pins);
    let control = Arc::new(Mutex::new(crate::control::sensor_control(driver)));
    let sensor: Arc<dyn SensorSide> = control.clone();
    let b: Arc<dyn BridgeDevice> = bridge.clone();
    let session = Session::new(
        b,
        sensor,
        SessionOptions {
            buffers: options.buffers,
            source: BufferSource::Memory(options.memory.clone()),
            max_error_frames: options.max_error_frames,
        },
    );
    let mut camera = NativeCamera::assemble(
        info,
        options,
        None,
        None,
        Arc::clone(&control),
        session,
        opened,
        None,
    );
    let truth = match &spec.truth {
        Some(path) => Some(std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| {
                    NativeError::InvalidConfig(format!("truth file {}: {e}", path.display()))
                })?,
        )),
        None => None,
    };
    let rig = Arc::new(SimRig {
        key: key.clone(),
        spec,
        scene,
        desc,
        bus,
        bridge,
        queue: Mutex::new(None),
        readout: Mutex::new(None),
        control,
        stop: AtomicBool::new(false),
        truth: Mutex::new(truth),
    });
    claim.0 = None;
    let weak = Arc::downgrade(&rig);
    std::thread::Builder::new()
        .name("styx-sim-sensor".into())
        .spawn(move || produce(&weak))
        .map_err(|e| NativeError::InvalidConfig(format!("the simulated sensor's thread: {e}")))?;
    camera.sim = Some(rig);
    Ok(camera)
}

/// [`NativeCamera::configure`] for a simulated camera: the sensor as on the device (power,
/// mode, flips, the start frame duration), the bridge's start request for it, and a capture
/// queue of the frame's size in the receiver's format.
pub(crate) fn configure(
    camera: &mut NativeCamera,
    settings: &StreamSettings,
) -> Result<Configured> {
    let rig = camera
        .sim
        .clone()
        .ok_or(NativeError::State("not simulated"))?;
    if camera.session.is_streaming() {
        return Err(NativeError::Busy(
            "stop streaming before configuring".into(),
        ));
    }
    let (hflip, vflip) = (settings.hflip, settings.vflip);
    check_flips(&camera.info.description, settings)?;
    let mut mode = select_mode(&camera.info.modes, settings, &camera.info.raw_formats)?.clone();
    let frame_length = {
        let mut c = lock(&camera.control);
        c.bring_up(&mode.mode, &mode.format)?;
        c.driver_mut().set_flips(hflip, vflip)?;
        start_frame_length(&mut c, settings)?
    };
    mode.code = camera.bus_code(mode.code, hflip, vflip);
    let link_freq = rig
        .desc
        .formats
        .get(&mode.format)
        .and_then(|f| f.link_frequency)
        .unwrap_or(0);
    let t = mode.timing;
    lock(&camera.control).expect_start(ExpectedStart {
        code: mode.code,
        width: mode.width,
        height: mode.height,
        link_freq: link_freq as i64,
    });
    rig.bridge.set_template(StreamRequest {
        link_freq: link_freq as i64,
        pixel_rate: t.pixel_rate as i64,
        code: mode.code,
        width: mode.width,
        height: mode.height,
        hblank: t.hblank as i32,
        vblank: frame_length.saturating_sub(t.height) as i32,
        ..idle_request()
    });
    // The memory format: the one asked for (in the pad's order), else the receiver's first.
    let order = styx_sensor::MbusCode(mode.code).color_filter();
    let want = settings.fourcc.map(|f| {
        order
            .and_then(|o| formats::with_color_filter(f, o))
            .unwrap_or(f)
    });
    let offered: Vec<FourCc> = formats::memory_formats(mode.code)
        .iter()
        .map(|f| f.fourcc)
        .collect();
    let fourcc = match want {
        Some(f) if offered.contains(&f) => f,
        Some(f) => {
            return Err(NativeError::InvalidConfig(format!(
                "the simulated receiver does not write {f} for this mode (offers {offered:?})"
            )));
        }
        None => formats::choose(mode.code, &offered)
            .ok_or_else(|| NativeError::InvalidConfig("no raw format for the mode".into()))?,
    };
    let raw = formats::raw_format(fourcc)
        .ok_or_else(|| NativeError::InvalidConfig(format!("{fourcc} is not a raw format")))?;
    // As rp1-cfe: lines aligned to 16 bytes.
    let stride = raw.packing.line_bytes(mode.width).next_multiple_of(16);
    let size_image = stride * mode.height;
    let desc = &rig.desc;
    let black = desc
        .pixel_array
        .black_level
        .map_or(0.0, |b| b.value as f32 / ((1u32 << b.bits) - 1) as f32);
    let array = desc.pixel_array.size;
    let [l, t0, w, h] = mode.crop;
    let field = [
        l as f32 / array.width as f32,
        t0 as f32 / array.height as f32,
        (l + w) as f32 / array.width as f32,
        (t0 + h) as f32 / array.height as f32,
    ];
    *lock(&rig.readout) = Some((
        Readout {
            width: mode.width,
            height: mode.height,
            stride: stride as usize,
            packing: raw.packing,
            bits: raw.bits,
            order: desc.pixel_array.color_filter,
            black,
        },
        field,
    ));
    let new_queue = {
        let mut q = lock(&rig.queue);
        match &*q {
            Some(queue) if queue.buffer_len() == size_image as usize => None,
            _ => {
                let queue = FakeQueue::new(Arc::clone(&rig.bridge), size_image as usize);
                queue.set_real_time(true);
                *q = Some(Arc::clone(&queue));
                Some(queue)
            }
        }
    };
    if let Some(queue) = new_queue {
        let video: Arc<dyn CaptureDevice> = queue.open();
        camera.session.attach_video(video, true)?;
    }
    // The frames' format names the order the data has (see `NativeCamera::configure`).
    let data_order = desc.color_filter(hflip, vflip);
    let configured = Configured {
        interval: crate::modes::frame_interval(&mode.timing, frame_length),
        mode,
        fourcc: formats::with_color_filter(fourcc, data_order).unwrap_or(fourcc),
        stride,
        size_image,
        frame_length,
        hflip,
        vflip,
    };
    camera.configured = Some(configured.clone());
    Ok(configured)
}

/// The producer's pace and fault state.
struct Pace {
    next: Option<Instant>,
    streaming_since: Option<Instant>,
    /// When the last capture error came, in stream time.
    last_fail: Option<Duration>,
    /// Lockstep: the stream's first timestamp and the next frame's time after it.
    clock: Option<(Duration, Duration)>,
    /// Lockstep: since when the next frame has waited for a free buffer.
    starved: Option<Instant>,
    rng: u64,
}

impl Pace {
    fn random(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The frame producer: runs while the rig lives.
fn produce(rig: &Weak<SimRig>) {
    let mut pace = Pace {
        next: None,
        streaming_since: None,
        last_fail: None,
        clock: None,
        starved: None,
        rng: 0x2545_F491_4F6C_DD1D,
    };
    let mut seeded = false;
    loop {
        let Some(r) = rig.upgrade() else {
            return;
        };
        if r.stop.load(Ordering::Acquire) {
            return;
        }
        if !seeded {
            pace.rng ^= r.spec.faults.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            seeded = true;
        }
        let wait = r.step(&mut pace);
        drop(r);
        if let Some(w) = wait {
            std::thread::sleep(w);
        }
    }
}

impl SimRig {
    /// One frame (or a wait): returns how long to wait before the next step.
    fn step(&self, pace: &mut Pace) -> Option<Duration> {
        let queue = lock(&self.queue).clone();
        let Some(queue) = queue.filter(|q| q.is_streaming()) else {
            pace.next = None;
            pace.streaming_since = None;
            pace.clock = None;
            pace.starved = None;
            pace.last_fail = None;
            return Some(Duration::from_millis(2));
        };
        let Some((readout, field)) = *lock(&self.readout) else {
            return Some(Duration::from_millis(2));
        };
        let now = Instant::now();
        let since = *pace.streaming_since.get_or_insert(now);
        let lockstep = self.spec.lockstep;
        if lockstep && !queue.has_free_buffer() {
            let starved = *pace.starved.get_or_insert(now);
            if now.duration_since(starved) < super::LOCKSTEP_STALL {
                return Some(Duration::from_millis(1));
            }
        }
        pace.starved = None;
        let seq = queue.next_sequence();
        let applied = lock(&self.control).applied(u64::from(seq));
        let (exposure, gain, period) = applied.map_or(
            (Duration::from_millis(10), 1.0, Duration::from_millis(33)),
            |a| (a.exposure, a.gain(), a.frame_duration),
        );
        let period = period.max(Duration::from_millis(1));
        // The frame starts on its due time (with jitter); far behind, the pace starts again.
        let due = match pace.next {
            Some(d) if d + period * 4 > now => d,
            _ => now,
        };
        let jitter = self.spec.faults.jitter.as_secs_f64();
        let offset = (pace.random() - 0.5) * 2.0 * jitter;
        let start = if offset >= 0.0 {
            due + Duration::from_secs_f64(offset)
        } else {
            due.checked_sub(Duration::from_secs_f64(-offset))
                .unwrap_or(due)
        };
        if start > now {
            pace.next = Some(due);
            return Some(start - now);
        }
        pace.next = Some(due + period);
        // The frame's timestamp and its time in the stream: lockstep, one period after the
        // last frame's (jitter included); else the clock's.
        let (stamp, elapsed) = if lockstep {
            let (base, sim) = *pace
                .clock
                .get_or_insert((styx_kernel::monotonic_now(), Duration::ZERO));
            pace.clock = Some((base, sim + period));
            let t = Duration::from_secs_f64((sim.as_secs_f64() + offset).max(0.0));
            (base + t, t)
        } else {
            (styx_kernel::monotonic_now(), now.duration_since(since))
        };
        let faults = &self.spec.faults;
        if let Some(every) = faults.unplug_every
            && elapsed >= every
        {
            // The sensor goes away: listed again after `unplug_for`.
            unplug(&self.key, faults.unplug_for);
            queue.unplug();
            self.bridge.unplug();
            pace.streaming_since = None;
            pace.clock = None;
            return Some(Duration::from_millis(2));
        }
        if let Some(every) = faults.fail_every
            && elapsed.saturating_sub(pace.last_fail.unwrap_or_default()) >= every
        {
            pace.last_fail = Some(elapsed);
            queue.fail();
            return None;
        }
        if faults.drop > 0.0 && pace.random() < faults.drop {
            queue.lose_frame();
            return None;
        }
        queue.set_corrupt(faults.corrupt > 0.0 && pace.random() < faults.corrupt);
        let flip = |f: &Option<styx_sensor::Flip>| {
            f.is_some_and(|f| self.bus.byte(f.address) & f.mask != 0)
        };
        let (hflip, vflip) = (
            flip(&self.desc.controls.hflip),
            flip(&self.desc.controls.vflip),
        );
        let ctx = FrameContext {
            index: u64::from(seq),
            time: elapsed.as_secs_f64(),
            exposure: exposure.as_secs_f64(),
            width: readout.width,
            height: readout.height,
            field,
        };
        let (scene, truth) = self.scene.frame_at(&ctx);
        if let Some(t) = truth {
            let mut out = lock(&self.truth);
            if let Some(file) = out.as_mut() {
                use std::io::Write;
                let line = truth_line(
                    seq,
                    stamp,
                    (readout.width, readout.height),
                    (hflip, vflip),
                    &t,
                );
                if file
                    .write_all(line.as_bytes())
                    .and_then(|()| file.flush())
                    .is_err()
                {
                    *out = None;
                }
            }
        }
        let e = Exposure {
            seconds: exposure.as_secs_f32(),
            gain: gain as f32,
            hflip,
            vflip,
            noise: self
                .spec
                .noise
                .then(|| faults.seed ^ u64::from(seq).wrapping_mul(0x2545_F491_4F6C_DD1D)),
        };
        queue.tick_at(Some(stamp), |buf, _| render(buf, &scene, &readout, &e));
        None
    }
}
