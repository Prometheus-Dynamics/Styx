//! The simulated camera drivers: a sensor, a receiver and an ISP, each speaking its Lemnos contract.
//!
//! Copied from Lemnos `testing/crates/camera-proof/tests/sim/mod.rs` at 04d09360 (test code there;
//! here it is a library module of `styx-sim`). The bus and the proof helpers are left out.
//!
//! The sensor and the receiver share one clock and one set of settings per camera (`silicon`),
//! found by the name the board gives them in their `config` table. The receiver latches and
//! publishes a frame when the camera grants it (see `silicon` for the clock). The sensor applies
//! each posted value on the frame the mode's delay says, and the frame carries sequence, identity
//! and size, and no pixel content: `styx-sim` renders the pixels into the lease it is given.
//!
//! The drivers keep no state across a restart. Each contract call that changes something is
//! appended to a log.

use std::sync::{Arc, Mutex, PoisonError};

use lemnos_contract_camera_isp::{
    CameraIsp, ConfigLayout, Counters as IspCounters, IspInfo, IspInput, IspOutput, OutputLayout,
};
use lemnos_contract_camera_receiver::{
    BayerOrder, CameraReceiver, Counters, FrameLayout, FrameStarts, Limits, LinkConfig,
    MAX_STREAM_NAME, Packing,
};
use lemnos_contract_camera_sensor::{
    AppliedLanding, CameraSensor, Controls, EmbeddedData, FrameValues, GainLaw, GainRange, InForce,
    Landing, Mode, Rect, SensorInfo,
};
use lemnos_core::{Config, Lifecycle, Result, Status, StreamHandle, driver};
use lemnos_hal::blocks::StreamFrame;

use crate::silicon::{self, SensorTiming, Silicon};

/// Calls in the order they arrived, across every simulated device in this process.
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

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

/// Modes as (width, height). Every mode has the same pixel rate, so a mode's frame time follows
/// from its link alone, and its frame length is that time over the line time. No rate is listed:
/// the mode's frame length and line time give it.
pub(crate) const MODES: [(u32, u32); 2] = [(320, 40), (640, 40)];
const PIXEL_RATE_HZ: u64 = 1_536_000;
/// How often the receiver looks for a granted frame while it waits (nanoseconds).
const POLL_NS: u64 = 200_000;

pub(crate) fn mode(index: usize, timing: SensorTiming) -> Option<Mode> {
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
    // The shortest frame the mode runs: the frame time the link implies, in whole lines (rounded
    // down, so the frame never runs slower than the link carries it).
    let line_time_ps = u64::from(silicon::LINE_TIME_PS as u32);
    let frame_length_min = frame_interval_ns(&link) * 1_000 / line_time_ps;
    let frame_length_min = u32::try_from(frame_length_min).ok()?;
    let max_fps_milli =
        u32::try_from(1_000_000_000_000_000u64 / (u64::from(frame_length_min) * line_time_ps))
            .ok()?;
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
        ..Mode::default()
    })
}

/// The shared clock named by a device's `camera` config entry.
fn clock(cfg: &Config<'_>) -> Result<Arc<Silicon>> {
    let name = cfg.str("camera").ok_or(Status::INVALID_ARGUMENT)?;
    silicon::lookup(name).ok_or(Status::NOT_FOUND)
}

#[driver("sim-sensor", provides = [CameraSensor])]
pub struct SimSensor {
    selected: Option<u16>,
    streaming: bool,
    applied: Option<FrameValues>,
    clock: Option<Arc<Silicon>>,
}

impl Lifecycle for SimSensor {
    type Needs = SimSensorNeeds;

    fn start(_needs: SimSensorNeeds, cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            selected: None,
            streaming: false,
            applied: None,
            clock: Some(clock(cfg)?),
        })
    }
}

impl SimSensor {
    fn silicon(&self) -> Result<&Silicon> {
        self.clock.as_deref().ok_or(Status::INVALID_ARGUMENT)
    }
}

impl CameraSensor for SimSensor {
    // The simulated sensor has no frame-start counts, so it refuses landings and the
    // landing reads (NOT_SUPPORTED, as a driver without them answers).
    fn post_values_at(&mut self, _values: FrameValues) -> Result<Landing> {
        Err(Status::NOT_SUPPORTED)
    }

    fn applied_landing(&mut self) -> Result<Option<AppliedLanding>> {
        Err(Status::NOT_SUPPORTED)
    }

    fn in_force(&mut self) -> Result<InForce> {
        Err(Status::NOT_SUPPORTED)
    }

    fn set_exposure(&mut self, _lines: u32) -> Result<Landing> {
        Err(Status::NOT_SUPPORTED)
    }

    fn set_analogue_gain(&mut self, _code: u32) -> Result<Landing> {
        Err(Status::NOT_SUPPORTED)
    }

    fn set_frame_length(&mut self, _lines: u32) -> Result<Landing> {
        Err(Status::NOT_SUPPORTED)
    }

    fn info(&mut self) -> Result<SensorInfo> {
        Ok(SensorInfo {
            modes: MODES.len() as u16,
            array_width: 1280,
            array_height: 800,
            test_patterns: 0,
            embedded_data: EmbeddedData::None,
        })
    }

    fn identity(&mut self, _into: &mut [u8]) -> Result<usize> {
        // No kernel names the simulated sensor, so its name is unknown and matches nothing.
        Err(Status::NOT_SUPPORTED)
    }

    fn mode(&mut self, index: u16) -> Result<Mode> {
        let timing = self.silicon()?.timing();
        mode(usize::from(index), timing).ok_or(Status::NOT_FOUND)
    }

    fn select_mode(&mut self, index: u16) -> Result<()> {
        if self.streaming {
            return Err(Status::BUSY);
        }
        if mode(usize::from(index), SensorTiming::default()).is_none() {
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
        let silicon = self.silicon()?;
        if let Some(code) = controls.exposure {
            silicon.set_now(silicon::EXPOSURE, Some(code));
        }
        if let Some(code) = controls.analogue_gain {
            silicon.set_now(silicon::GAIN, Some(code));
        }
        if let Some(code) = controls.frame_length {
            silicon.set_now(silicon::FRAME_LENGTH, Some(code));
        }
        Ok(())
    }

    /// Posts `values` during the last latched frame: each control shows from that frame plus its
    /// delay. Exposure and gain posted together show together only when their delays are equal.
    fn post_values(&mut self, values: FrameValues) -> Result<()> {
        if self.selected.is_none() {
            return Err(Status::INVALID_ARGUMENT);
        }
        let silicon = self.silicon()?;
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

#[driver("sim-receiver", provides = [CameraReceiver])]
pub struct SimReceiver {
    #[needs(free)]
    frames: StreamHandle,
    configured: Option<LinkConfig>,
    layout: Option<FrameLayout>,
    /// Frame time of the link, and the timestamp of the next frame made, while running.
    running: Option<Running>,
    counters: Counters,
    clock: Option<Arc<Silicon>>,
}

#[derive(Clone, Copy)]
struct Running {
    interval_ns: u64,
    next_ns: Option<u64>,
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
            clock: Some(clock(cfg)?),
        })
    }

    /// Latches and publishes each frame the camera has granted, and asks to be looked at again
    /// soon: a frame is made when it is granted, not when its time comes.
    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        let running = self.running?;
        let silicon = self.clock.clone()?;
        let size = self.layout.map_or(0, |layout| layout.size);
        let mut next = running.next_ns.unwrap_or(now_ns);
        while let Some(id) = silicon.latch() {
            match self.frames.next_buffer() {
                Ok(Some(buffer)) => {
                    let frame = StreamFrame {
                        buffer,
                        bytes: size,
                        sequence: id,
                        identity: id,
                        timestamp_ns: next,
                    };
                    match self.frames.publish(frame) {
                        Ok(()) => self.counters.frames += 1,
                        Err(_) => self.counters.errors += 1,
                    }
                }
                Ok(None) => self.counters.dropped += 1,
                Err(_) => self.counters.errors += 1,
            }
            next = next.saturating_add(running.interval_ns);
        }
        if let Some(running) = self.running.as_mut() {
            running.next_ns = Some(next);
        }
        Some(now_ns.saturating_add(POLL_NS))
    }
}

impl CameraReceiver for SimReceiver {
    fn frame_starts(&mut self) -> Result<Option<FrameStarts>> {
        Err(Status::NOT_SUPPORTED)
    }

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
        log("receiver.start");
        self.counters = Counters::default();
        self.running = Some(Running {
            interval_ns: frame_interval_ns(&link),
            next_ns: None,
        });
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        log("receiver.stop");
        self.running = None;
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

    fn source(&mut self, _into: &mut [u8]) -> Result<usize> {
        // No kernel describes the simulated input, so what is wired to it is unknown.
        Err(Status::NOT_SUPPORTED)
    }

    fn layout(&mut self) -> Result<Option<FrameLayout>> {
        Ok(self.layout)
    }
}

/// Frame time in nanoseconds for a link: its active pixels at its pixel rate.
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
    fn input_layout(&mut self) -> Result<Option<OutputLayout>> {
        Err(Status::NOT_SUPPORTED)
    }

    fn output_layout(&mut self, _output: u8) -> Result<Option<OutputLayout>> {
        Err(Status::NOT_SUPPORTED)
    }

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

/// A board with one sensor, one receiver it links to, and the ISP that receiver links to. A decoy
/// receiver with no link is listed before the real one, so a tool that takes the first receiver
/// fails. `sensor_link` replaces the sensor's link when given. Every simulated device names the
/// camera `camera`, whose clock and timing they share.
pub(crate) fn board(camera: &str, sensor_link: Option<&str>) -> String {
    let link = match sensor_link {
        Some(target) => format!("link = \"{target}\""),
        None => String::new(),
    };
    format!(
        "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-camera\"\n\
         [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
         [[devices]]\nid = \"csi-spare\"\ndriver = \"sim-receiver\"\n\
         config = {{ camera = \"{camera}\" }}\n\
         [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
         [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\n\
         config = {{ camera = \"{camera}\" }}\n\
         [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
         [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\n{link}\n\
         config = {{ camera = \"{camera}\" }}\n"
    )
}
