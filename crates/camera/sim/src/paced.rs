//! Free-running simulated camera drivers on Lemnos's simulated backend: a sensor, a receiver and an
//! ISP, each speaking its contract, registered by a test's own `lemnosd` (`entry::<SimSensor>()`).
//! The receiver makes frames on the host's ticks at the rate its link implies (paced, not lock-step:
//! for the lock-step camera see [`crate::SimCamera`]). Frames carry sequence, identity, timestamp and
//! size, and no pixel content. This is the one definition the crates' tests share (it replaces the
//! copies each test tree kept); it goes when Lemnos's simulated camera chain lands (TODO S1).
//!
//! What a board's `config` turns on, per device:
//! - `camera = "<name>"` (sensor and receiver): the two share a frame clock, so a posted value
//!   lands on the frame its delay names, `in_force` and the landings answer, and requests are fitted
//!   to the mode with `CLAMP_*` bits. Without it the sensor has no landings (`NOT_SUPPORTED`) and
//!   keeps a posted value as given.
//! - `identity = "<name>"` (sensor): the name the kernel gives it; absent, `identity` is
//!   `NOT_SUPPORTED`.
//! - `source = "<name>"` (receiver): the sensor wired to its input; absent, `NOT_SUPPORTED`; empty,
//!   nothing is wired.
//! - `vary_bytes = true` (receiver): frame `n` is [`frame_bytes`] long, so a reader can check each
//!   frame's own length.
//!
//! Each mode drops two frames, and the receiver has fault switches ([`FAIL_STARTS`],
//! [`STALL_NOW`]) a test sets. Every call that changes something is logged ([`take_log`]).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lemnos_contract_camera_isp::{
    CameraIsp, ConfigLayout, Counters as IspCounters, IspInfo, IspInput, IspOutput, OutputLayout,
};
use lemnos_contract_camera_receiver::{
    BayerOrder, CameraReceiver, Counters, FrameLayout, Limits, LinkConfig, MAX_STREAM_NAME, Packing,
};
use lemnos_contract_camera_sensor::frame_length_for_fps_milli;
use lemnos_contract_camera_sensor::{
    CameraSensor, Controls, FrameValues, GainRange, Mode, Rect, SensorInfo,
};
use lemnos_core::{
    Config, ConfigError, Health, Key, Kind, Lifecycle, Result, Status, StreamHandle, driver,
};

/// The settings the simulated sensor reads (see the module's list).
const SENSOR_KEYS: &[Key] = &[
    Key::optional("camera", Kind::Str),
    Key::optional("identity", Kind::Str),
];
/// The settings the simulated receiver reads.
const RECEIVER_KEYS: &[Key] = &[
    Key::optional("camera", Kind::Str),
    Key::optional("source", Kind::Str),
    Key::optional("vary_bytes", Kind::Bool),
];

/// Bytes in frame `sequence` of a stream whose frames are at most `size` bytes, on a receiver set
/// to `vary_bytes`: frames vary by sequence (as a compressed stream's do).
#[must_use]
pub fn frame_bytes(size: u32, sequence: u64) -> u32 {
    size.saturating_sub((sequence % 4) as u32 * 64)
}

/// Writes `name` into `into`, returning its length; `BUFFER_TOO_SMALL` when it does not fit.
fn write_name(name: &str, into: &mut [u8]) -> Result<usize> {
    into.get_mut(..name.len())
        .ok_or(Status::BUFFER_TOO_SMALL)?
        .copy_from_slice(name.as_bytes());
    Ok(name.len())
}
use lemnos_hal::blocks::StreamFrame;

/// Lanes of the frame clock: exposure, analogue gain, frame length.
const LANES: usize = 3;

/// One camera's frame clock, shared by its sensor and its receiver through the `camera` config
/// entry. A write the sensor takes at frame `w` is on the list until the receiver makes the frame
/// it shows from (`w` plus the mode's delay for that control); then it is in force. The receiver
/// makes frames as the stream runs, so a write lands on the frame its delay names, as the sensor's
/// contract says, as long as the consumer posts it before that frame is made.
#[derive(Default)]
struct Clock {
    state: Mutex<ClockState>,
}

#[derive(Default)]
struct ClockState {
    /// Writes not yet in force: (lane, code, first frame they show on), in the order they came.
    pending: Vec<(usize, u32, u64)>,
    /// The codes in force, per lane.
    in_force: [Option<u32>; LANES],
    /// The frame the receiver made last (the frame in progress), since its start.
    last: Option<u64>,
}

/// Every camera's clock in this process, by its `camera` name. Each board text names its cameras
/// uniquely (see `two_cameras` in `service.rs`), so tests in one process do not share a clock.
static CLOCKS: Mutex<Vec<(String, Arc<Clock>)>> = Mutex::new(Vec::new());

/// The clock of the camera a device's `camera` config entry names. A device with no entry has a
/// clock of its own: its writes land on its own receiver's frames only if it is given a name.
fn clock_named(cfg: &Config<'_>) -> Result<Arc<Clock>> {
    let Some(name) = cfg.str("camera") else {
        return Ok(Arc::new(Clock::default()));
    };
    let mut all = CLOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, clock)) = all.iter().find(|(n, _)| n == name) {
        return Ok(Arc::clone(clock));
    }
    let clock = Arc::new(Clock::default());
    all.push((name.to_string(), Arc::clone(&clock)));
    Ok(clock)
}

impl Clock {
    /// Puts in force every write that shows from frame `frame` on, as the receiver makes it.
    fn make_frame(&self, frame: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.last = Some(frame);
        let mut i = 0;
        while i < state.pending.len() {
            if state.pending[i].2 <= frame {
                let (lane, code, _) = state.pending.remove(i);
                state.in_force[lane] = Some(code);
            } else {
                i += 1;
            }
        }
    }

    /// Puts a write in force now (a control set before the stream, not a posted value).
    fn set_now(&self, lane: usize, code: u32) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.in_force[lane] = Some(code);
    }

    /// The frame the receiver is in (its last start), or `None` before it has started one.
    fn frame_in_progress(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last
    }

    fn in_force(&self) -> [Option<u32>; LANES] {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .in_force
    }

    fn post(&self, lane: usize, code: u32, shows_from: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.pending.push((lane, code, shows_from));
    }

    /// Forgets the writes not yet in force, as a sensor that starts again has none.
    fn reset(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.pending.clear();
        state.last = None;
    }
}

/// Calls in the order they arrived, across every simulated device.
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Receiver starts that fail with `Status::IO` before one succeeds. Set by a test.
pub static FAIL_STARTS: AtomicU32 = AtomicU32::new(0);
/// While set, a running receiver makes no frames. Cleared when the receiver is stopped. Set by a test.
pub static STALL_NOW: AtomicBool = AtomicBool::new(false);
/// While set, every receiver reads as gone (`Health::Gone`: what it was bound with is dead): the
/// host drops it and binds it again under the failure rule, and with the board's `auto_off_after`
/// turns it off for good and ends its stream as device-failed. Set by a test.
pub static GONE_RECEIVER: AtomicBool = AtomicBool::new(false);
/// When set, the next receiver tick ends its frame stream's consumer for breaking the rules
/// (`fault_consumer`), and clears it. Set by a test.
pub static FAULT_CONSUMER: AtomicBool = AtomicBool::new(false);

/// A number no earlier call in this process returned, for naming a board's cameras: devices share a
/// frame clock by camera name, process-wide, so two test files that each counted their boards from
/// 0 would share clocks when their tests run in one process.
#[must_use]
pub fn next_board() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Takes the calls logged since the last call, in arrival order.
#[must_use]
pub fn take_log() -> Vec<String> {
    std::mem::take(&mut *LOG.lock().unwrap_or_else(PoisonError::into_inner))
}

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
        line_time_ps: 9_100_000,
        line_length: 1456,
        // The shortest frame at this rate: the contract's whole line nearest to the rate.
        frame_length_min: frame_length_for_fps_milli(9_100_000, 0, 1, u32::MAX, fps * 1_000)
            .map_or(0, |(lines, _)| lines),
        frame_length_max: 52_000,
        exposure_min: 16,
        exposure_margin: 25,
        exposure_fraction_bits: 4,
        analogue_gain: GainRange {
            min: 16,
            max: 255,
            unity: 16,
            step: 1,
            law: lemnos_contract_camera_sensor::GainLaw::Linear,
        },
        digital_gain: GainRange::default(),
        exposure_delay: 2,
        gain_delay: 1,
        frame_length_delay: 2,
        frame_offset_ps: 0,
        readout_ps: 0,
        drop_frames: 2,
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
    /// The frame clock the receiver of this camera makes frames on.
    clock: Arc<Clock>,
    /// Whether the board named the camera (`camera`): only then does the clock land posts.
    framed: bool,
    /// The `CLAMP_*` bits of the last request.
    clamped: u8,
    /// The name the kernel gives this sensor (`identity`), if the board says.
    identity: Option<String>,
}

impl SimSensor {
    /// Writes a post during frame `written`: each control shows from `written` plus its delay.
    fn post_written(&mut self, values: FrameValues, written: u64) {
        let fit = self.fit(values.controls);
        let m = self.selected.and_then(|i| mode(usize::from(i)));
        if let Some(m) = m {
            for (lane, code) in [fit.exposure, fit.analogue_gain, fit.frame_length]
                .into_iter()
                .enumerate()
            {
                if let Some(code) = code {
                    let shows = written + Self::delay_of(&m, lane);
                    self.clock.post(lane, code, shows);
                }
            }
        }
        self.applied = Some(FrameValues {
            identity: values.identity,
            controls: fit,
        });
    }

    /// The request as the sensor takes it: each value moved to the nearest one the selected mode
    /// allows (the exposure ceiling is the frame less the mode's margin, in the frame length asked
    /// for or in force), with the `CLAMP_*` bits of what was moved.
    fn fit(&mut self, controls: Controls) -> Controls {
        let m = self.selected.and_then(|i| mode(usize::from(i)));
        let Some(m) = m else {
            return controls;
        };
        let written = self.clock.in_force();
        let mut clamped = 0;
        let frame_length = controls.frame_length.map(|lines| {
            let fit = lines.clamp(m.frame_length_min, m.frame_length_max);
            if fit != lines {
                clamped |= lemnos_contract_camera_sensor::CLAMP_FRAME_LENGTH;
            }
            fit
        });
        let frame_length_now = frame_length.or(written[2]).unwrap_or(m.frame_length_min);
        let exposure_max =
            (frame_length_now.saturating_sub(m.exposure_margin) << 4).max(m.exposure_min);
        let asked = match controls.exposure {
            Some(code) => Some(code),
            None if frame_length.is_some() => written[0],
            None => None,
        };
        let exposure = asked.map(|code| {
            let fit = code.clamp(m.exposure_min, exposure_max);
            if fit != code {
                clamped |= lemnos_contract_camera_sensor::CLAMP_EXPOSURE;
            }
            fit
        });
        let analogue_gain = controls.analogue_gain.map(|code| {
            let fit = code.clamp(m.analogue_gain.min, m.analogue_gain.max);
            if fit != code {
                clamped |= lemnos_contract_camera_sensor::CLAMP_ANALOGUE_GAIN;
            }
            fit
        });
        self.clamped = clamped;
        Controls {
            exposure,
            analogue_gain,
            frame_length,
            digital_gain: controls.digital_gain,
        }
    }

    /// The mode's delay, in frames, for a lane: a write at frame `w` shows from `w + delay`.
    fn delay_of(m: &Mode, lane: usize) -> u64 {
        u64::from(match lane {
            0 => m.exposure_delay,
            1 => m.gain_delay,
            _ => m.frame_length_delay,
        })
    }
}

impl Lifecycle for SimSensor {
    type Needs = SimSensorNeeds;
    fn check_config<'a>(cfg: &Config<'a>) -> core::result::Result<(), ConfigError<'a>> {
        cfg.check_keys(SENSOR_KEYS)
    }

    fn start(_needs: SimSensorNeeds, cfg: &Config<'_>) -> Result<Self> {
        let clock = clock_named(cfg)?;
        clock.reset();
        Ok(Self {
            selected: None,
            streaming: false,
            applied: None,
            clock,
            framed: cfg.str("camera").is_some(),
            clamped: 0,
            identity: cfg.str("identity").map(str::to_string),
        })
    }
}

impl CameraSensor for SimSensor {
    /// Posts as `post_values` does, written during the frame the receiver is in (its launch), and
    /// answers the landing by the contract's rule: the launch frame plus each control's delay.
    /// `BUSY` before the receiver has counted a frame since its start.
    fn post_values_at(
        &mut self,
        values: lemnos_contract_camera_sensor::FrameValues,
    ) -> core::result::Result<lemnos_contract_camera_sensor::Landing, lemnos_core::Status> {
        if !self.framed {
            return Err(Status::NOT_SUPPORTED);
        }
        let launch = self.clock.frame_in_progress().ok_or(Status::BUSY)?;
        let m = self
            .selected
            .and_then(|i| mode(usize::from(i)))
            .ok_or(Status::INVALID_ARGUMENT)?;
        self.post_written(values, launch);
        let delays = [0, 1, 2].map(|lane| Self::delay_of(&m, lane) as u8);
        Ok(lemnos_contract_camera_sensor::landing_of(
            launch,
            delays,
            &values.controls,
        ))
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
        if !self.framed {
            return Err(Status::NOT_SUPPORTED);
        }
        let [exposure, gain, frame_length] = self.clock.in_force();
        Ok(lemnos_contract_camera_sensor::InForce {
            exposure_lines: exposure.map(|code| code >> 4),
            analogue_gain: gain,
            frame_length,
            clamped: self.clamped,
        })
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

    fn set_controls(&mut self, controls: Controls) -> Result<()> {
        if self.selected.is_none() {
            return Err(Status::INVALID_ARGUMENT);
        }
        log("sensor.set_controls");
        if !self.framed {
            return Ok(());
        }
        let fit = self.fit(controls);
        // A control set before the stream runs is in force at once.
        for (lane, code) in [fit.exposure, fit.analogue_gain, fit.frame_length]
            .into_iter()
            .enumerate()
        {
            if let Some(code) = code {
                self.clock.set_now(lane, code);
            }
        }
        Ok(())
    }

    /// A posted value is written during frame `identity` and shows from `identity` plus the mode's
    /// delay for each control (the frame clock puts it in force then).
    fn post_values(&mut self, values: FrameValues) -> Result<()> {
        if self.framed {
            self.post_written(values, values.identity);
        } else {
            self.applied = Some(values);
        }
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
    /// The frame clock of this camera: the frames this receiver makes put the writes in force.
    clock: Arc<Clock>,
    /// Whether the board named the camera (`camera`): only then are frame starts answered.
    framed: bool,
    /// The name of the sensor wired to this receiver's input (`source`), if the board says.
    source: Option<String>,
    /// Frames vary in length by sequence ([`frame_bytes`]).
    vary_bytes: bool,
}

impl Lifecycle for SimReceiver {
    type Needs = SimReceiverNeeds;
    fn check_config<'a>(cfg: &Config<'a>) -> core::result::Result<(), ConfigError<'a>> {
        cfg.check_keys(RECEIVER_KEYS)
    }

    fn health(&self) -> Health {
        if GONE_RECEIVER.load(Ordering::Acquire) {
            Health::Gone("the simulated receiver is set to be gone")
        } else {
            Health::Ok
        }
    }
    const TICKS: bool = true;

    fn start(needs: SimReceiverNeeds, cfg: &Config<'_>) -> Result<Self> {
        Ok(Self {
            frames: needs.frames,
            configured: None,
            layout: None,
            run: None,
            counters: Counters::default(),
            clock: clock_named(cfg)?,
            framed: cfg.str("camera").is_some(),
            source: cfg.str("source").map(str::to_string),
            vary_bytes: cfg.bool("vary_bytes").unwrap_or(false),
        })
    }

    /// Makes every frame due by `now_ns`, and returns when the next one is due.
    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        if self.run.is_some() && FAULT_CONSUMER.swap(false, Ordering::AcqRel) {
            let _ = self.frames.fault_consumer();
        }
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
            // The frame is made first: the writes that show from it are in force on it.
            self.clock.make_frame(run.sequence);
            // A stalled receiver makes no frame: the fault switch is on.
            if STALL_NOW.load(Ordering::Acquire) {
                run.sequence += 1;
                next = next.saturating_add(run.interval_ns);
                continue;
            }
            match self.frames.next_buffer() {
                Ok(Some(buffer)) => {
                    let frame = StreamFrame {
                        buffer,
                        bytes: if self.vary_bytes {
                            frame_bytes(size, run.sequence)
                        } else {
                            size
                        },
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
    /// The frames the receiver has made since its start: the frame in progress is `count - 1`. The
    /// latest start's time is not kept by this double (0).
    fn frame_starts(
        &mut self,
    ) -> core::result::Result<
        Option<lemnos_contract_camera_receiver::FrameStarts>,
        lemnos_core::Status,
    > {
        if !self.framed {
            return Err(Status::NOT_SUPPORTED);
        }
        Ok(self.clock.frame_in_progress().map(|frame| {
            lemnos_contract_camera_receiver::FrameStarts {
                count: frame + 1,
                latest_ns: 0,
            }
        }))
    }

    fn embedded_stream(&mut self, _into: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    // Added at Lemnos de395ab4: a simulated receiver has no video inputs, so NOT_SUPPORTED.
    fn inputs(&mut self, _into: &mut [u8]) -> Result<usize> {
        Err(Status::NOT_SUPPORTED)
    }

    fn input(&mut self) -> Result<u32> {
        Err(Status::NOT_SUPPORTED)
    }

    fn source(&mut self, into: &mut [u8]) -> Result<usize> {
        let name = self.source.as_deref().ok_or(Status::NOT_SUPPORTED)?;
        write_name(name, into)
    }

    fn layout(&mut self) -> Result<Option<lemnos_contract_camera_receiver::FrameLayout>> {
        Ok(self.layout)
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
        if FAIL_STARTS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Status::IO);
        }
        log("receiver.start");
        self.clock.reset();
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
        STALL_NOW.store(false, Ordering::Release);
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

/// A driver's state is the daemon's to read through its contract; `Debug` names the driver only.
macro_rules! debug_by_name {
    ($($ty:ident),*) => {$(
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($ty)).finish_non_exhaustive()
            }
        }
    )*};
}
debug_by_name!(SimSensor, SimReceiver, SimIsp);

/// What a board text from [`board_from`] has: the sensor's link, and the `config` entries the module
/// lists.
#[derive(Debug, Clone, Copy, Default)]
pub struct Board<'a> {
    /// The device the sensor links to (`link`), if any.
    pub sensor_link: Option<&'a str>,
    /// The sensor's `identity`.
    pub identity: Option<&'a str>,
    /// `csi0`'s `source`.
    pub source: Option<&'a str>,
    /// `csi0`'s `vary_bytes`.
    pub vary_bytes: bool,
    /// `csi0`'s `auto_off_after`: failed attempts in a row before the host turns it off for good.
    pub receiver_auto_off_after: Option<u32>,
}

/// A board text for `lemnosd`: an ISP `isp0`, a decoy receiver `csi-spare` with no link (listed
/// first, so a tool that takes the first receiver fails), a receiver `csi0` linked to `isp0`, and a
/// sensor `cam0-sensor`, as `spec` says.
#[must_use]
pub fn board_from(spec: Board<'_>) -> String {
    let link = spec
        .sensor_link
        .map(|target| format!("link = \"{target}\"\n"))
        .unwrap_or_default();
    let identity = spec
        .identity
        .map(|name| format!("config = {{ identity = \"{name}\" }}\n"))
        .unwrap_or_default();
    let mut receiver = Vec::new();
    if let Some(name) = spec.source {
        receiver.push(format!("source = \"{name}\""));
    }
    if spec.vary_bytes {
        receiver.push("vary_bytes = true".to_string());
    }
    let mut receiver = if receiver.is_empty() {
        String::new()
    } else {
        format!("config = {{ {} }}\n", receiver.join(", "))
    };
    if let Some(after) = spec.receiver_auto_off_after {
        receiver.push_str(&format!("auto_off_after = {after}\n"));
    }
    format!(
        "format = \"lemnos.board\"\nschema_version = 1\n[board]\nid = \"sim-camera\"\n\
         [[devices]]\nid = \"isp0\"\ndriver = \"sim-isp\"\n\
         [[devices]]\nid = \"csi-spare\"\ndriver = \"sim-receiver\"\n\
         [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
         [[devices]]\nid = \"csi0\"\ndriver = \"sim-receiver\"\nlink = \"isp0\"\n\
         {receiver}\
         [devices.streams.frames]\nbuffers = 8\nbuffer_bytes = 32000\n\
         [[devices]]\nid = \"cam0-sensor\"\ndriver = \"sim-sensor\"\n{link}{identity}"
    )
}

/// [`board_from`] with the sensor linked to `sensor_link` and nothing else set.
#[must_use]
pub fn board(sensor_link: Option<&str>) -> String {
    board_from(Board {
        sensor_link,
        ..Board::default()
    })
}

/// [`board_from`] with the sensor's `identity` and `csi0`'s `source`.
#[must_use]
pub fn board_with(
    sensor_link: Option<&str>,
    identity: Option<&str>,
    source: Option<&str>,
) -> String {
    board_from(Board {
        sensor_link,
        identity,
        source,
        ..Board::default()
    })
}

/// [`board`] linked to `csi0`, with a second sensor `cam1-sensor` linked to `csi0` as well.
#[must_use]
pub fn board_with_two_sensors() -> String {
    format!(
        "{}[[devices]]\nid = \"cam1-sensor\"\ndriver = \"sim-sensor\"\nlink = \"csi0\"\n",
        board(Some("csi0"))
    )
}
