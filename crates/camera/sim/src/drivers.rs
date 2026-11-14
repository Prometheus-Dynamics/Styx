//! The simulated camera drivers: a sensor, a receiver and an ISP, each speaking its Lemnos contract.
//!
//! Copied from Lemnos `testing/crates/camera-proof/tests/sim/mod.rs` at 04d09360 (test code there;
//! here it is a library module of `styx-sim`). The bus and the proof helpers are left out.
//!
//! The drivers take their behaviour from the device's `config` table (see the keys below). With a
//! `camera` key the sensor and the receiver share that camera's clock (`silicon`): the receiver
//! latches and publishes a frame when the camera grants it, and the sensor applies each posted value
//! on the frame the mode's delay says. Without one (a timed device, as the test simulators of
//! `styx-native`, `styx-service` and `styx-lemnos` used) the receiver makes a frame each frame
//! interval of its link, and the sensor only records what it is given. The frame carries sequence,
//! identity and size, and no pixel content: `styx-sim` renders the pixels into the lease it is given.
//!
//! Config keys (all optional; the defaults are the lock-step camera's behaviour):
//! - sensor: `camera` (the clock), `identity` (the name the kernel gives it), `modes` (`"link"`,
//!   `"run"` or `"bare"`: which fields a mode reports, see [`Fields`]), `drop_frames` (frames a mode
//!   drops after start).
//! - receiver: `camera` (the clock), `source` (the sensor wired to its input), `layout` (`false`
//!   reports no layout), `frames` (`"varied"`: each frame's bytes vary by sequence), `frame_ids`
//!   (`"zero"`: every frame's identity is 0).
//!
//! The drivers keep no state across a restart. Each contract call that changes something is
//! appended to a log.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lemnos_contract_camera_isp::{
    CameraIsp, ConfigLayout, Counters as IspCounters, IspInfo, IspInput, IspOutput, OutputLayout,
};
use lemnos_contract_camera_receiver::{
    BayerOrder, CameraReceiver, Counters, FrameLayout, Limits, LinkConfig, MAX_STREAM_NAME, Packing,
};
use lemnos_contract_camera_sensor::{
    CameraSensor, Controls, EmbeddedData, FrameValues, GainLaw, GainRange, Mode, Rect, SensorInfo,
};
use lemnos_core::{Config, Lifecycle, Result, Status, StreamHandle, driver};
use lemnos_hal::blocks::StreamFrame;

use crate::silicon::{self, SensorTiming, Silicon};

/// Calls in the order they arrived, across every simulated device in this process.
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Receiver starts that fail with `Status::IO` before one succeeds. Set by a test.
pub static FAIL_STARTS: AtomicU32 = AtomicU32::new(0);
/// While set, a running receiver makes no frames. Cleared when the receiver is stopped. Set by a test.
pub static STALL_NOW: AtomicBool = AtomicBool::new(false);

fn log(entry: &str) {
    LOG.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(entry.to_string());
}

/// The contract calls the simulated drivers have taken, in arrival order, across every simulated
/// camera in this process.
pub fn call_log() -> Vec<String> {
    LOG.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Takes the calls logged since the last take, in arrival order.
pub fn take_log() -> Vec<String> {
    std::mem::take(&mut *LOG.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Modes as (width, height). Every mode has the same pixel rate, so a mode's frame time follows
/// from its link alone, and its frame length is that time over the line time. No rate is listed:
/// the mode's frame length and line time give it.
pub(crate) const MODES: [(u32, u32); 2] = [(320, 40), (640, 40)];
const PIXEL_RATE_HZ: u64 = 1_536_000;
/// How often the receiver looks for a granted frame while it waits (nanoseconds).
const POLL_NS: u64 = 200_000;

/// Which fields a mode reports. `Link` is the lock-step camera's mode: every field, the delays of
/// its timing and the largest frame rate its line time allows. `Run` is the mode the `styx-native-run`
/// test simulator reports: every field, the same delays, and no largest frame rate. `Bare` is the
/// mode the `styx-native` and `styx-lemnos` test simulators report: the link, the crop, the gains
/// and the largest frame rate they listed, and nothing else (no line time, no delays).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fields {
    Link,
    Run,
    Bare,
}

impl Fields {
    fn parse(config: Option<&str>) -> Result<Self> {
        match config {
            None | Some("link") => Ok(Self::Link),
            Some("run") => Ok(Self::Run),
            Some("bare") => Ok(Self::Bare),
            Some(_) => Err(Status::INVALID_ARGUMENT),
        }
    }
}

/// The largest frame rate the `bare` modes list, in frames per second (test data only: it reports
/// a rate, it does not pace anything).
const BARE_FPS: [u32; 2] = [120, 60];

pub(crate) fn mode(
    index: usize,
    timing: SensorTiming,
    fields: Fields,
    drop_frames: u8,
) -> Option<Mode> {
    let (width, height) = *MODES.get(index)?;
    let link = LinkConfig {
        lanes: 2,
        virtual_channel: 0,
        data_type: 0x2b,
        bit_depth: 10,
        continuous_clock: false,
        width,
        height,
        pixel_rate_hz: PIXEL_RATE_HZ,
        link_rate_mbps: 1000,
    };
    if fields == Fields::Bare {
        let fps = *BARE_FPS.get(index)?;
        return Some(Mode {
            link,
            bayer: BayerOrder::Rggb,
            max_fps_milli: fps * 1000,
            crop: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            analogue_gain: GainRange::default(),
            digital_gain: GainRange::default(),
            ..Mode::default()
        });
    }
    // The shortest frame the mode runs: the frame time the link implies, in whole lines (rounded
    // down, so the frame never runs slower than the link carries it).
    let line_time_ps = u64::from(silicon::LINE_TIME_PS as u32);
    let frame_length_min = frame_interval_ns(&link) * 1_000 / line_time_ps;
    let frame_length_min = u32::try_from(frame_length_min).ok()?;
    let max_fps_milli = match fields {
        Fields::Link => {
            u32::try_from(1_000_000_000_000_000u64 / (u64::from(frame_length_min) * line_time_ps))
                .ok()?
        }
        _ => 0,
    };
    Some(Mode {
        link,
        bayer: BayerOrder::Rggb,
        max_fps_milli,
        line_time_ps: line_time_ps as u32,
        line_length: 1456,
        frame_length_min,
        frame_length_max: 52_000,
        exposure_min: 16,
        exposure_margin: 25,
        exposure_fraction_bits: silicon::EXPOSURE_FRACTION_BITS,
        crop: Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        analogue_gain: GainRange {
            min: 16,
            max: 255,
            unity: silicon::GAIN_UNITY,
            step: 1,
            law: GainLaw::Linear,
        },
        digital_gain: GainRange::default(),
        exposure_delay: timing.exposure_delay,
        gain_delay: timing.gain_delay,
        frame_length_delay: timing.frame_length_delay,
        drop_frames,
        ..Mode::default()
    })
}

/// The shared clock named by a device's `camera` config entry, if it has one.
fn clock(cfg: &Config<'_>) -> Result<Option<Arc<Silicon>>> {
    match cfg.str("camera") {
        None => Ok(None),
        Some(name) => silicon::lookup(name).map(Some).ok_or(Status::NOT_FOUND),
    }
}

/// Writes `name` into `into`, returning its length; `BUFFER_TOO_SMALL` when it does not fit.
fn write_name(name: &str, into: &mut [u8]) -> Result<usize> {
    into.get_mut(..name.len())
        .ok_or(Status::BUFFER_TOO_SMALL)?
        .copy_from_slice(name.as_bytes());
    Ok(name.len())
}

#[driver("sim-sensor", provides = [CameraSensor])]
pub struct SimSensor {
    selected: Option<u16>,
    streaming: bool,
    applied: Option<FrameValues>,
    clock: Option<Arc<Silicon>>,
    fields: Fields,
    drop_frames: u8,
    identity: Option<String>,
}

impl std::fmt::Debug for SimSensor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimSensor")
            .field("selected", &self.selected)
            .field("streaming", &self.streaming)
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

impl Lifecycle for SimSensor {
    type Needs = SimSensorNeeds;

    fn start(_needs: SimSensorNeeds, cfg: &Config<'_>) -> Result<Self> {
        let drop_frames = match cfg.int("drop_frames") {
            None => 0,
            Some(n) => u8::try_from(n).map_err(|_| Status::INVALID_ARGUMENT)?,
        };
        Ok(Self {
            selected: None,
            streaming: false,
            applied: None,
            clock: clock(cfg)?,
            fields: Fields::parse(cfg.str("modes"))?,
            drop_frames,
            identity: cfg.str("identity").map(str::to_string),
        })
    }
}

impl CameraSensor for SimSensor {
    fn info(&mut self) -> Result<SensorInfo> {
        Ok(SensorInfo {
            modes: MODES.len() as u16,
            array_width: 1280,
            array_height: 800,
            test_patterns: 0,
            embedded_data: EmbeddedData::None,
        })
    }

    fn identity(&mut self, into: &mut [u8]) -> Result<usize> {
        // No kernel names the simulated sensor unless its config does: then its name is the one given.
        let name = self.identity.as_deref().ok_or(Status::NOT_SUPPORTED)?;
        write_name(name, into)
    }

    fn mode(&mut self, index: u16) -> Result<Mode> {
        let timing = match &self.clock {
            Some(silicon) => silicon.timing(),
            None => SensorTiming::default(),
        };
        mode(usize::from(index), timing, self.fields, self.drop_frames).ok_or(Status::NOT_FOUND)
    }

    fn select_mode(&mut self, index: u16) -> Result<()> {
        if self.streaming {
            return Err(Status::BUSY);
        }
        if mode(
            usize::from(index),
            SensorTiming::default(),
            self.fields,
            self.drop_frames,
        )
        .is_none()
        {
            return Err(Status::NOT_FOUND);
        }
        log("sensor.select_mode");
        self.selected = Some(index);
        Ok(())
    }

    fn selected(&mut self) -> Result<Option<u16>> {
        Ok(self.selected)
    }

    fn start(&mut self) -> Result<()> {
        if self.selected.is_none() {
            return Err(Status::INVALID_ARGUMENT);
        }
        log("sensor.start");
        self.streaming = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        log("sensor.stop");
        self.streaming = false;
        Ok(())
    }

    /// Writes `controls` to show on the next frame to be latched.
    fn set_controls(&mut self, controls: Controls) -> Result<()> {
        if self.selected.is_none() {
            return Err(Status::INVALID_ARGUMENT);
        }
        log("sensor.set_controls");
        if let Some(silicon) = self.clock.as_deref() {
            if let Some(code) = controls.exposure {
                silicon.set_now(silicon::EXPOSURE, Some(code));
            }
            if let Some(code) = controls.analogue_gain {
                silicon.set_now(silicon::GAIN, Some(code));
            }
            if let Some(code) = controls.frame_length {
                silicon.set_now(silicon::FRAME_LENGTH, Some(code));
            }
        }
        Ok(())
    }

    /// Posts `values` during the last latched frame: each control shows from that frame plus its
    /// delay. Exposure and gain posted together show together only when their delays are equal.
    /// A timed sensor only records them.
    fn post_values(&mut self, values: FrameValues) -> Result<()> {
        if let Some(silicon) = self.clock.as_deref() {
            if self.selected.is_none() {
                return Err(Status::INVALID_ARGUMENT);
            }
            let controls = values.controls;
            if let Some(code) = controls.exposure {
                silicon.post(silicon::EXPOSURE, Some(code));
            }
            if let Some(code) = controls.analogue_gain {
                silicon.post(silicon::GAIN, Some(code));
            }
            if let Some(code) = controls.frame_length {
                silicon.post(silicon::FRAME_LENGTH, Some(code));
            }
        }
        self.applied = Some(values);
        Ok(())
    }

    fn applied(&mut self) -> Result<Option<FrameValues>> {
        Ok(self.applied)
    }

    fn set_flips(&mut self, _horizontal: bool, _vertical: bool) -> Result<BayerOrder> {
        Err(Status::NOT_SUPPORTED)
    }

    fn set_test_pattern(&mut self, code: u16) -> Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(Status::INVALID_ARGUMENT)
        }
    }
}

/// The receiver's frame clock while it runs: the interval between frames, the time of the next
/// frame a timed receiver makes (`None` before its first tick), and the sequence of the next frame.
#[derive(Clone, Copy)]
struct Running {
    interval_ns: u64,
    next_ns: Option<u64>,
    sequence: u64,
}

#[driver("sim-receiver", provides = [CameraReceiver])]
pub struct SimReceiver {
    #[needs]
    frames: StreamHandle,
    configured: Option<LinkConfig>,
    layout: Option<FrameLayout>,
    running: Option<Running>,
    counters: Counters,
    clock: Option<Arc<Silicon>>,
    source: Option<String>,
    reports_layout: bool,
    varied: bool,
    zero_identity: bool,
}

impl std::fmt::Debug for SimReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimReceiver")
            .field("configured", &self.configured)
            .field("running", &self.running.is_some())
            .finish_non_exhaustive()
    }
}

impl Lifecycle for SimReceiver {
    type Needs = SimReceiverNeeds;
    const TICKS: bool = true;

    fn start(needs: SimReceiverNeeds, cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            frames: needs.frames,
            configured: None,
            layout: None,
            running: None,
            counters: Counters::default(),
            clock: clock(cfg)?,
            source: cfg.str("source").map(str::to_string),
            reports_layout: cfg.bool("layout").unwrap_or(true),
            varied: cfg.str("frames") == Some("varied"),
            zero_identity: cfg.str("frame_ids") == Some("zero"),
        })
    }

    /// Latches and publishes each frame the camera has granted, and asks to be looked at again
    /// soon: a frame is made when it is granted, not when its time comes. A timed receiver makes
    /// each frame when its time comes, and asks to be looked at when the next one is due.
    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        let running = self.running?;
        match self.clock.clone() {
            Some(silicon) => {
                let mut next = running.next_ns.unwrap_or(now_ns);
                while let Some(id) = silicon.latch() {
                    self.publish(id, next);
                    next = next.saturating_add(running.interval_ns);
                }
                if let Some(running) = self.running.as_mut() {
                    running.next_ns = Some(next);
                }
                Some(now_ns.saturating_add(POLL_NS))
            }
            None => self.tick_timed(running, now_ns),
        }
    }
}

impl SimReceiver {
    /// Publishes one frame: its sequence and timestamp, and the identity and bytes the config asks for.
    fn publish(&mut self, sequence: u64, timestamp_ns: u64) {
        let size = self.layout.map_or(0, |layout| layout.size);
        let bytes = if self.varied {
            frame_bytes(size, sequence)
        } else {
            size
        };
        let identity = if self.zero_identity { 0 } else { sequence };
        match self.frames.next_buffer() {
            Ok(Some(buffer)) => {
                let frame = StreamFrame {
                    buffer,
                    bytes,
                    sequence,
                    identity,
                    timestamp_ns,
                };
                match self.frames.publish(frame) {
                    Ok(()) => self.counters.frames += 1,
                    Err(_) => self.counters.errors += 1,
                }
            }
            Ok(None) => self.counters.dropped += 1,
            Err(_) => self.counters.errors += 1,
        }
    }

    /// Makes every frame due by `now_ns`, and returns when the next one is due.
    fn tick_timed(&mut self, mut run: Running, now_ns: u64) -> Option<u64> {
        let mut next = match run.next_ns {
            Some(next) => next,
            None => {
                let next = now_ns.saturating_add(run.interval_ns);
                run.next_ns = Some(next);
                self.running = Some(run);
                return Some(next);
            }
        };
        while next <= now_ns {
            // A stalled receiver makes no frame: the fault switch is on.
            if STALL_NOW.load(Ordering::Acquire) {
                run.sequence += 1;
                next = next.saturating_add(run.interval_ns);
                continue;
            }
            self.publish(run.sequence, next);
            run.sequence += 1;
            next = next.saturating_add(run.interval_ns);
        }
        run.next_ns = Some(next);
        self.running = Some(run);
        Some(next)
    }
}

impl CameraReceiver for SimReceiver {
    fn limits(&mut self) -> Result<Limits> {
        Ok(Limits {
            max_lanes: 2,
            max_link_rate_mbps: 1500,
            max_width: 4096,
            max_height: 4096,
        })
    }

    fn configure(&mut self, link: LinkConfig, packing: Packing) -> Result<FrameLayout> {
        if self.running.is_some() {
            return Err(Status::BUSY);
        }
        if link.lanes == 0
            || link.lanes > 2
            || link.width == 0
            || link.height == 0
            || link.pixel_rate_hz == 0
            || !matches!(link.bit_depth, 8 | 10 | 12)
        {
            return Err(Status::INVALID_ARGUMENT);
        }
        log("receiver.configure");
        let line = match packing {
            Packing::Packed => (u64::from(link.width) * u64::from(link.bit_depth)).div_ceil(8),
            Packing::Unpacked16 => u64::from(link.width) * 2,
        };
        let stride = line.next_multiple_of(16);
        let layout = FrameLayout {
            width: link.width,
            height: link.height,
            bit_depth: link.bit_depth,
            packing,
            stride: u32::try_from(stride).map_err(|_| Status::INVALID_ARGUMENT)?,
            size: u32::try_from(stride * u64::from(link.height))
                .map_err(|_| Status::INVALID_ARGUMENT)?,
        };
        self.configured = Some(link);
        self.layout = Some(layout);
        Ok(layout)
    }

    fn link(&mut self) -> Result<Option<LinkConfig>> {
        Ok(self.configured)
    }

    fn frame_stream(&mut self, into: &mut [u8]) -> Result<usize> {
        let name = b"frames";
        debug_assert!(name.len() <= MAX_STREAM_NAME);
        into.get_mut(..name.len())
            .ok_or(Status::BUFFER_TOO_SMALL)?
            .copy_from_slice(name);
        Ok(name.len())
    }

    fn start(&mut self) -> Result<()> {
        let link = self.configured.ok_or(Status::INVALID_ARGUMENT)?;
        if self.running.is_some() {
            return Err(Status::BUSY);
        }
        if FAIL_STARTS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Status::IO);
        }
        log("receiver.start");
        self.counters = Counters::default();
        self.running = Some(Running {
            interval_ns: frame_interval_ns(&link),
            next_ns: None,
            sequence: 0,
        });
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        log("receiver.stop");
        self.running = None;
        STALL_NOW.store(false, Ordering::Release);
        Ok(())
    }

    fn counters(&mut self) -> Result<Counters> {
        Ok(self.counters)
    }

    fn feeds(&mut self, _into: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn embedded_stream(&mut self, _into: &mut [u8]) -> Result<usize> {
        // The simulated receiver captures no embedded lines.
        Ok(0)
    }

    fn source(&mut self, into: &mut [u8]) -> Result<usize> {
        // No kernel describes the simulated input unless its config does: then what is wired to it.
        let name = self.source.as_deref().ok_or(Status::NOT_SUPPORTED)?;
        write_name(name, into)
    }

    fn layout(&mut self) -> Result<Option<FrameLayout>> {
        Ok(if self.reports_layout {
            self.layout
        } else {
            None
        })
    }
}

/// Bytes in frame `sequence` of a stream whose frames are at most `size` bytes, when the config
/// asks for varied frames (as a compressed stream's are): a reader can check each frame's length.
pub fn frame_bytes(size: u32, sequence: u64) -> u32 {
    size.saturating_sub((sequence % 4) as u32 * 64)
}

/// Frame time in nanoseconds for a link: its active pixels at its pixel rate.
///
/// PACING: the real frame period follows the frame length (lines of line time), and Lemnos will
/// provide the period function. This is the stand-in for the timed receivers only. No second
/// formula is to be written here.
fn frame_interval_ns(link: &LinkConfig) -> u64 {
    let pixels = u128::from(link.width) * u128::from(link.height);
    let rate = u128::from(link.pixel_rate_hz.max(1));
    u64::try_from(pixels * 1_000_000_000 / rate)
        .unwrap_or(u64::MAX)
        .max(1)
}

#[driver("sim-isp", provides = [CameraIsp])]
pub struct SimIsp {
    input: Option<IspInput>,
    running: bool,
}

impl std::fmt::Debug for SimIsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimIsp")
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

impl Lifecycle for SimIsp {
    type Needs = SimIspNeeds;

    fn start(_needs: SimIspNeeds, _cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            input: None,
            running: false,
        })
    }
}

impl CameraIsp for SimIsp {
    fn info(&mut self) -> Result<IspInfo> {
        Ok(IspInfo {
            max_outputs: 0,
            max_width: 4096,
            max_height: 4096,
            config_layouts: 0,
            statistics_layout: 0,
            statistics_bytes: 0,
        })
    }

    fn config_layout(&mut self, _index: u8) -> Result<ConfigLayout> {
        Err(Status::NOT_FOUND)
    }

    fn set_input(&mut self, input: IspInput) -> Result<()> {
        if self.running {
            return Err(Status::BUSY);
        }
        log("isp.set_input");
        self.input = Some(input);
        Ok(())
    }

    fn set_output(&mut self, _output: u8, _image: IspOutput) -> Result<OutputLayout> {
        Err(Status::NOT_FOUND)
    }

    fn clear_output(&mut self, _output: u8) -> Result<()> {
        Err(Status::NOT_FOUND)
    }

    fn config_stream(&mut self, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_SUPPORTED)
    }

    fn statistics_stream(&mut self, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_SUPPORTED)
    }

    fn input_stream(&mut self, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_SUPPORTED)
    }

    fn output_stream(&mut self, _output: u8, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_FOUND)
    }

    fn start(&mut self) -> Result<()> {
        if self.input.is_none() {
            return Err(Status::INVALID_ARGUMENT);
        }
        log("isp.start");
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        log("isp.stop");
        self.running = false;
        Ok(())
    }

    fn counters(&mut self) -> Result<IspCounters> {
        Ok(IspCounters::default())
    }
}
