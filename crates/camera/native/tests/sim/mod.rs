//! Copied from styx-lemnos's test simulator (itself copied from Lemnos `testing/crates/camera-proof`
//! at 04d09360): test code, not a library. Simulated camera drivers, a sensor, a receiver and an ISP,
//! each speaking its contract.
//!
//! They keep no state across a restart. Each contract call that changes something is appended to
//! a log, so a test can check the order calls arrived in across devices. The receiver makes frames
//! on the host's ticks at the rate its link implies, and publishes them on the frame stream the
//! board gives it. A driver's stream end can't write pixel bytes (on hardware the block's DMA does
//! that), so simulated frames carry sequence, identity, timestamp and size, and no pixel content.

use std::sync::{Mutex, PoisonError};

use lemnos_contract_camera_isp::{
    CameraIsp, ConfigLayout, Counters as IspCounters, IspInfo, IspInput, IspOutput, OutputLayout,
};
use lemnos_contract_camera_receiver::{
    BayerOrder, CameraReceiver, Counters, FrameLayout, Limits, LinkConfig, MAX_STREAM_NAME, Packing,
};
use lemnos_contract_camera_sensor::{
    CameraSensor, Controls, FrameValues, GainRange, Mode, Rect, SensorInfo,
};
use lemnos_core::{Config, Lifecycle, Result, Status, StreamHandle, driver};
use lemnos_hal::blocks::StreamFrame;

/// Calls in the order they arrived, across every simulated device.
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn log(entry: &str) {
    LOG.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(entry.to_string());
}

/// Modes as (width, height, frames per second). Every mode has the same pixel rate, so frame time
/// follows from the link alone.
const MODES: [(u32, u32, u32); 2] = [(320, 40, 120), (640, 40, 60)];
const PIXEL_RATE_HZ: u64 = 1_536_000;

fn mode(index: usize) -> Option<Mode> {
    let (width, height, fps) = *MODES.get(index)?;
    Some(Mode {
        link: LinkConfig {
            lanes: 2,
            virtual_channel: 0,
            data_type: 0x2b,
            bit_depth: 10,
            continuous_clock: false,
            width,
            height,
            pixel_rate_hz: PIXEL_RATE_HZ,
            link_rate_mbps: 1000,
        },
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
    })
}

/// Frame time in nanoseconds for a link: its active pixels at its pixel rate.
fn frame_interval_ns(link: &LinkConfig) -> u64 {
    let pixels = u128::from(link.width) * u128::from(link.height);
    let rate = u128::from(link.pixel_rate_hz.max(1));
    u64::try_from(pixels * 1_000_000_000 / rate)
        .unwrap_or(u64::MAX)
        .max(1)
}

#[driver("sim-sensor", provides = [CameraSensor])]
pub struct SimSensor {
    selected: Option<u16>,
    streaming: bool,
    applied: Option<FrameValues>,
    /// The name the kernel gives this sensor (`identity`), from the board's `config`. Absent: no
    /// kernel names it, so `identity` is `NOT_SUPPORTED`.
    identity: Option<String>,
}

impl Lifecycle for SimSensor {
    type Needs = SimSensorNeeds;

    fn start(_needs: SimSensorNeeds, cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            selected: None,
            streaming: false,
            applied: None,
            identity: cfg.str("identity").map(str::to_string),
        })
    }
}

impl CameraSensor for SimSensor {
    // Added at Lemnos 58098f97: this simulated driver has no landings, so it answers NOT_SUPPORTED.
    fn post_values_at(
        &mut self,
        _values: lemnos_contract_camera_sensor::FrameValues,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn applied_landing(
        &mut self,
    ) -> core::result::Result<
        Option<lemnos_contract_camera_sensor::AppliedLanding>,
        lemnos_core::Status,
    > {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn in_force(
        &mut self,
    ) -> core::result::Result<lemnos_contract_camera_sensor::InForce, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn set_exposure(
        &mut self,
        _lines: u32,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn set_analogue_gain(
        &mut self,
        _code: u32,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn set_frame_length(
        &mut self,
        _lines: u32,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn identity(&mut self, into: &mut [u8]) -> Result<usize> {
        let name = self.identity.as_deref().ok_or(Status::NOT_SUPPORTED)?;
        write_name(name, into)
    }
    fn info(&mut self) -> Result<SensorInfo> {
        Ok(SensorInfo {
            modes: MODES.len() as u16,
            array_width: 1280,
            array_height: 800,
            test_patterns: 0,
            embedded_data: lemnos_contract_camera_sensor::EmbeddedData::None,
        })
    }

    fn mode(&mut self, index: u16) -> Result<Mode> {
        mode(usize::from(index)).ok_or(Status::NOT_FOUND)
    }

    fn select_mode(&mut self, index: u16) -> Result<()> {
        if self.streaming {
            return Err(Status::BUSY);
        }
        if mode(usize::from(index)).is_none() {
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

    fn set_controls(&mut self, _controls: Controls) -> Result<()> {
        if self.selected.is_none() {
            return Err(Status::INVALID_ARGUMENT);
        }
        log("sensor.set_controls");
        Ok(())
    }

    fn post_values(&mut self, values: FrameValues) -> Result<()> {
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

/// A frame stream's producer, and what it makes: one frame per interval, while running.
#[derive(Clone, Copy)]
struct Run {
    interval_ns: u64,
    next_ns: Option<u64>,
    sequence: u64,
}

#[driver("sim-receiver", provides = [CameraReceiver])]
pub struct SimReceiver {
    #[needs(free)]
    frames: StreamHandle,
    configured: Option<LinkConfig>,
    layout: Option<FrameLayout>,
    run: Option<Run>,
    counters: Counters,
    /// The name of the sensor wired to this receiver's input (`source`), from the board's
    /// `config`. Absent: the kernel does not describe the input, so `source` is `NOT_SUPPORTED`.
    /// Present but empty: nothing is wired.
    source: Option<String>,
}

/// Writes `name` into `into`, returning its length; `BUFFER_TOO_SMALL` when it does not fit.
fn write_name(name: &str, into: &mut [u8]) -> Result<usize> {
    into.get_mut(..name.len())
        .ok_or(Status::BUFFER_TOO_SMALL)?
        .copy_from_slice(name.as_bytes());
    Ok(name.len())
}

impl Lifecycle for SimReceiver {
    type Needs = SimReceiverNeeds;
    const TICKS: bool = true;

    fn start(needs: SimReceiverNeeds, cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            frames: needs.frames,
            configured: None,
            layout: None,
            run: None,
            counters: Counters::default(),
            source: cfg.str("source").map(str::to_string),
        })
    }

    /// Makes every frame due by `now_ns`, and returns when the next one is due.
    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        let run = self.run.as_mut()?;
        let mut next = match run.next_ns {
            Some(next) => next,
            None => {
                let next = now_ns.saturating_add(run.interval_ns);
                run.next_ns = Some(next);
                return Some(next);
            }
        };
        let size = self.layout.map_or(0, |layout| layout.size);
        while next <= now_ns {
            match self.frames.next_buffer() {
                Ok(Some(buffer)) => {
                    let frame = StreamFrame {
                        buffer,
                        bytes: size,
                        sequence: run.sequence,
                        identity: run.sequence,
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
            run.sequence += 1;
            next = next.saturating_add(run.interval_ns);
        }
        run.next_ns = Some(next);
        Some(next)
    }
}

impl CameraReceiver for SimReceiver {
    // Added at Lemnos 58098f97: no frame-start counts here, so NOT_SUPPORTED.
    fn frame_starts(
        &mut self,
    ) -> core::result::Result<
        Option<lemnos_contract_camera_receiver::FrameStarts>,
        lemnos_core::Status,
    > {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn embedded_stream(&mut self, _into: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn source(&mut self, into: &mut [u8]) -> Result<usize> {
        let name = self.source.as_deref().ok_or(Status::NOT_SUPPORTED)?;
        write_name(name, into)
    }

    fn layout(&mut self) -> Result<Option<lemnos_contract_camera_receiver::FrameLayout>> {
        Ok(None)
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
        if self.run.is_some() {
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
        if self.run.is_some() {
            return Err(Status::BUSY);
        }
        log("receiver.start");
        self.counters = Counters::default();
        self.run = Some(Run {
            interval_ns: frame_interval_ns(&link),
            next_ns: None,
            sequence: 0,
        });
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        log("receiver.stop");
        self.run = None;
        Ok(())
    }

    fn counters(&mut self) -> Result<Counters> {
        Ok(self.counters)
    }

    fn feeds(&mut self, _into: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
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
    // Added at Lemnos 58098f97: no layout answers here, so NOT_SUPPORTED.
    fn input_layout(
        &mut self,
    ) -> core::result::Result<Option<lemnos_contract_camera_isp::OutputLayout>, lemnos_core::Status>
    {
        Err(lemnos_core::Status::NOT_SUPPORTED)
    }

    fn output_layout(
        &mut self,
        _output: u8,
    ) -> core::result::Result<Option<lemnos_contract_camera_isp::OutputLayout>, lemnos_core::Status>
    {
        Err(lemnos_core::Status::NOT_SUPPORTED)
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
/// fails. `sensor_link` replaces the sensor's link when given.
pub fn board(sensor_link: Option<&str>) -> String {
    board_with(sensor_link, None, None)
}

/// [`board`] with the sensor's `identity` and the receiver's `source` set in their `config`, when
/// given (see [`SimSensor`] and [`SimReceiver`]).
pub fn board_with(
    sensor_link: Option<&str>,
    identity: Option<&str>,
    source: Option<&str>,
) -> String {
    let link = match sensor_link {
        Some(target) => format!("link = \"{target}\""),
        None => String::new(),
    };
    let identity = match identity {
        Some(name) => format!("config = {{ identity = \"{name}\" }}"),
        None => String::new(),
    };
    let source = match source {
        Some(name) => format!("config = {{ source = \"{name}\" }}\n"),
        None => String::new(),
    };
    format!(
        "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-camera\"\n\
         [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
         [[devices]]\nid = \"csi-spare\"\ndriver = \"sim-receiver\"\n\
         [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
         [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\n\
         {source}\
         [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
         [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\n{link}\n{identity}\n"
    )
}

/// A board with two sensors, both linked to the receiver.
pub fn board_with_two_sensors() -> String {
    format!(
        "{}[[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\n",
        board(Some("csi0"))
    )
}
