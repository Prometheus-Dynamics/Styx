//! The userspace sensor driver: runs a description over a [`RegisterBus`] and [`SensorPins`].

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::bus::{RegisterBus, SensorPins};
use crate::desc::{Field, Flip, RegWrite, SensorDescription, Step};
use crate::error::{Result, SensorError};
use crate::gain::split_gain;
use crate::mbus::{ColorFilter, MbusCode};
use crate::schedule::{
    Applied, Control, ControlScheduler, ControlSet, ExposureLimit, IssueBatch, Landing, Mismatch,
};
use crate::timing::Timing;

/// Power and streaming state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverState {
    /// Not powered.
    Off,
    /// Powered, not streaming.
    Powered,
    /// Streaming.
    Streaming,
}

/// The applied mode.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveMode {
    /// Mode name.
    pub mode: String,
    /// Format name.
    pub format: String,
    /// Media bus code.
    pub code: MbusCode,
    /// Timing at the current horizontal blanking.
    pub timing: Timing,
}

/// Typed control values for a frame. Unset fields are left unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ControlRequest {
    /// Exposure time.
    pub exposure: Option<Duration>,
    /// Total gain (analogue first, then digital).
    pub gain: Option<f64>,
    /// Frame duration (sets the frame length).
    pub frame_duration: Option<Duration>,
}

/// The typed values that produced a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppliedControls {
    /// Frame sequence number.
    pub frame: u64,
    /// Exposure time.
    pub exposure: Duration,
    /// Exposure in lines.
    pub exposure_lines: f64,
    /// Analogue gain.
    pub analog_gain: f64,
    /// Digital gain (1 when the sensor has none).
    pub digital_gain: f64,
    /// Frame length in lines.
    pub frame_length: u32,
    /// Frame duration.
    pub frame_duration: Duration,
    /// The register codes, and which of them were read back.
    pub codes: Applied,
}

/// A register-driven sensor.
#[derive(Debug)]
pub struct SensorDriver<B, P> {
    desc: Arc<SensorDescription>,
    bus: B,
    pins: P,
    state: DriverState,
    mode: Option<ActiveMode>,
    flips: (bool, bool),
    scheduler: Option<ControlScheduler>,
}

impl<B: RegisterBus, P: SensorPins> SensorDriver<B, P> {
    /// A driver for a described sensor. Nothing is written until [`Self::power_up`].
    pub fn new(desc: Arc<SensorDescription>, bus: B, pins: P) -> Self {
        let flips = (
            desc.controls.hflip.is_some_and(|f| f.default),
            desc.controls.vflip.is_some_and(|f| f.default),
        );
        Self {
            desc,
            bus,
            pins,
            state: DriverState::Off,
            mode: None,
            flips,
            scheduler: None,
        }
    }

    /// The description.
    pub fn description(&self) -> &SensorDescription {
        &self.desc
    }

    /// The register bus.
    pub fn bus(&self) -> &B {
        &self.bus
    }

    /// The register bus, mutably.
    pub fn bus_mut(&mut self) -> &mut B {
        &mut self.bus
    }

    /// The pins.
    pub fn pins(&self) -> &P {
        &self.pins
    }

    /// Take the bus and pins back.
    pub fn into_parts(self) -> (B, P) {
        (self.bus, self.pins)
    }

    /// Current state.
    pub fn state(&self) -> DriverState {
        self.state
    }

    /// The applied mode.
    pub fn mode(&self) -> Option<&ActiveMode> {
        self.mode.as_ref()
    }

    /// The control scheduler (after [`Self::set_mode`]).
    pub fn scheduler(&self) -> Option<&ControlScheduler> {
        self.scheduler.as_ref()
    }

    /// Output colour filter order with the current flips.
    pub fn color_filter(&self) -> ColorFilter {
        self.desc.color_filter(self.flips.0, self.flips.1)
    }

    fn run(&mut self, steps: &[Step]) -> Result<()> {
        let mut batch: Vec<RegWrite> = Vec::new();
        for step in steps {
            if let Step::Write(w) = step {
                batch.push(*w);
                continue;
            }
            self.flush(&mut batch)?;
            let desc = Arc::clone(&self.desc);
            let (what, role, optional, res) = match step {
                Step::Delay(d) => {
                    self.pins.delay(*d);
                    continue;
                }
                Step::Gpio {
                    role,
                    value,
                    optional,
                } => ("gpio", role, *optional, self.pins.set_gpio(role, *value)),
                Step::Clock { role, on, optional } => {
                    let rate = on.then(|| desc.sensor.clocks.get(role).copied().unwrap_or(0));
                    ("clock", role, *optional, self.pins.set_clock(role, rate))
                }
                Step::Supply { role, on, optional } => {
                    ("supply", role, *optional, self.pins.set_supply(role, *on))
                }
                Step::Write(_) => unreachable!("handled above"),
            };
            match res {
                Err(e) if optional && e.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(SensorError::Pins {
                        what,
                        role: role.clone(),
                        source,
                    });
                }
                Ok(()) => {}
            }
        }
        self.flush(&mut batch)
    }

    fn flush(&mut self, batch: &mut Vec<RegWrite>) -> Result<()> {
        if let Some(first) = batch.first() {
            let address = first.address;
            self.bus
                .write_sequence(batch)
                .map_err(|source| SensorError::Bus {
                    op: "write sequence at",
                    address,
                    source,
                })?;
            batch.clear();
        }
        Ok(())
    }

    fn write_field(&mut self, field: &Field, value: u32) -> Result<()> {
        let current = if field.read_modify_write {
            Some(self.read(field.address, field.bytes)?)
        } else {
            None
        };
        let reg = field.encode(value, current);
        self.bus
            .write(field.address, field.bytes, reg)
            .map_err(|source| SensorError::Bus {
                op: "write",
                address: field.address,
                source,
            })
    }

    fn read(&mut self, address: u16, bytes: u8) -> Result<u32> {
        self.bus
            .read(address, bytes)
            .map_err(|source| SensorError::Bus {
                op: "read",
                address,
                source,
            })
    }

    fn require(&self, ok: bool, msg: &'static str) -> Result<()> {
        if ok {
            Ok(())
        } else {
            Err(SensorError::State(msg))
        }
    }

    /// Run the power-up sequence.
    pub fn power_up(&mut self) -> Result<()> {
        self.require(self.state == DriverState::Off, "power_up: already powered")?;
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.power_up)?;
        self.state = DriverState::Powered;
        Ok(())
    }

    /// Stop streaming if needed and run the power-down sequence.
    pub fn power_down(&mut self) -> Result<()> {
        if self.state == DriverState::Streaming {
            self.stop_streaming()?;
        }
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.power_down)?;
        self.state = DriverState::Off;
        self.mode = None;
        self.scheduler = None;
        Ok(())
    }

    /// Read the chip id and check it. Returns the value read.
    pub fn verify_chip_id(&mut self) -> Result<u32> {
        self.require(
            self.state != DriverState::Off,
            "verify_chip_id: not powered",
        )?;
        let Some(id) = self.desc.sensor.chip_id.clone() else {
            return Ok(0);
        };
        let found = self.read(id.address, id.bytes)?;
        if id.values.contains(&found) {
            Ok(found)
        } else {
            Err(SensorError::ChipId {
                expected: id.values,
                found,
            })
        }
    }

    /// Write the common init registers.
    pub fn init(&mut self) -> Result<()> {
        self.require(
            self.state == DriverState::Powered,
            "init: not powered or streaming",
        )?;
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.init)
    }

    /// Apply a mode and format: format registers, mode registers, line and frame length,
    /// default exposure and gain, and flips. Starts a new control schedule.
    pub fn set_mode(&mut self, mode: &str, format: &str) -> Result<&ActiveMode> {
        self.require(
            self.state == DriverState::Powered,
            "set_mode: not powered or streaming",
        )?;
        let desc = Arc::clone(&self.desc);
        let m = desc.mode(mode)?;
        let f = desc.format_for(m, format)?;
        let timing = Timing::for_mode(m, f, &desc.controls.exposure)
            .with_extra_lines(desc.controls.frame_length_extra_lines);
        let ctl = &desc.controls;
        ctl.frame_length
            .ok_or(SensorError::NoRegister("frame length"))?;
        self.run(&f.registers)?;
        self.run(&m.registers)?;
        if let Some(ll) = ctl.line_length {
            self.write_field(
                &ll.register,
                timing.line_length() / ll.pixels_per_unit.max(1),
            )?;
        }
        let fl = timing.frame_length_default();
        let exposure = ctl
            .exposure
            .default
            .min(fl.saturating_sub(ctl.exposure.margin))
            .max(ctl.exposure.min);
        let mut initial = ControlSet::new()
            .with(Control::FrameLength, fl)
            .with(Control::Exposure, exposure << ctl.exposure.fraction_bits)
            .with(Control::AnalogGain, ctl.analog_gain.default_code);
        if let Some(dg) = &ctl.digital_gain {
            initial.set(Control::DigitalGain, dg.default_code);
        }
        self.write_controls(&initial, false)?;
        let (h, v) = self.flips;
        self.write_flips(h, v)?;
        self.scheduler = Some(
            ControlScheduler::new(ctl.delays, initial).with_exposure_limit(ExposureLimit {
                min: ctl.exposure.min,
                margin: ctl.exposure.margin,
                fraction_bits: ctl.exposure.fraction_bits,
            }),
        );
        self.mode = Some(ActiveMode {
            mode: m.name.clone(),
            format: format.to_owned(),
            code: f.code,
            timing,
        });
        Ok(self.mode.as_ref().expect("just set"))
    }

    /// Change horizontal blanking (not while streaming). Returns the new timing.
    pub fn set_hblank(&mut self, hblank: u32) -> Result<Timing> {
        self.require(
            self.state == DriverState::Powered,
            "set_hblank: not powered or streaming",
        )?;
        let ll = self
            .desc
            .controls
            .line_length
            .ok_or(SensorError::NoRegister("line length"))?;
        let mode = self
            .mode
            .as_mut()
            .ok_or(SensorError::State("set_hblank: no mode"))?;
        mode.timing = mode.timing.with_hblank(hblank);
        let t = mode.timing;
        self.write_field(&ll.register, t.line_length() / ll.pixels_per_unit.max(1))?;
        Ok(t)
    }

    /// Write control codes (frame length first), inside group hold when asked and available.
    fn write_controls(&mut self, set: &ControlSet, hold: bool) -> Result<()> {
        if set.is_empty() {
            return Ok(());
        }
        let desc = Arc::clone(&self.desc);
        let ctl = &desc.controls;
        let gh = ctl.group_hold.as_ref().filter(|_| hold);
        if let Some(gh) = gh {
            self.run(&gh.start)?;
        }
        for (c, v) in set.iter() {
            let (field, value) = match c {
                Control::FrameLength => (
                    ctl.frame_length
                        .ok_or(SensorError::NoRegister("frame length"))?,
                    v,
                ),
                Control::Exposure => {
                    let f = ctl
                        .exposure
                        .register
                        .ok_or(SensorError::NoRegister("exposure"))?;
                    let fb = ctl.exposure.fraction_bits;
                    (
                        Field {
                            shift: f.shift - fb,
                            bits: Some(f.bits() + fb),
                            ..f
                        },
                        v,
                    )
                }
                Control::AnalogGain => (
                    ctl.analog_gain
                        .register
                        .ok_or(SensorError::NoRegister("analogue gain"))?,
                    v,
                ),
                Control::DigitalGain => {
                    let g = ctl.digital_gain.as_ref().and_then(|g| g.register);
                    (g.ok_or(SensorError::NoRegister("digital gain"))?, v)
                }
            };
            self.write_field(&field, value)?;
        }
        if let Some(gh) = gh {
            self.run(&gh.end)?;
            self.run(&gh.launch)?;
        }
        Ok(())
    }

    fn write_flip(&mut self, flip: Option<Flip>, on: bool) -> Result<()> {
        let Some(f) = flip else { return Ok(()) };
        let current = self.read(f.address, 1)?;
        let mask = u32::from(f.mask);
        let value = if on { current | mask } else { current & !mask };
        self.bus
            .write(f.address, 1, value)
            .map_err(|source| SensorError::Bus {
                op: "write",
                address: f.address,
                source,
            })
    }

    fn write_flips(&mut self, h: bool, v: bool) -> Result<()> {
        self.write_flip(self.desc.controls.hflip, h)?;
        self.write_flip(self.desc.controls.vflip, v)
    }

    /// Set horizontal mirror and vertical flip (not while streaming when the Bayer order
    /// changes).
    pub fn set_flips(&mut self, hflip: bool, vflip: bool) -> Result<()> {
        self.require(self.state != DriverState::Off, "set_flips: not powered")?;
        let changes = self.desc.color_filter(hflip, vflip) != self.color_filter();
        self.require(
            !(changes && self.state == DriverState::Streaming),
            "set_flips: would change the Bayer order while streaming",
        )?;
        self.write_flips(hflip, vflip)?;
        self.flips = (hflip, vflip);
        Ok(())
    }

    /// Select a test pattern by name (`off` disables it).
    pub fn set_test_pattern(&mut self, name: &str) -> Result<()> {
        self.require(
            self.state != DriverState::Off,
            "set_test_pattern: not powered",
        )?;
        let tp = self
            .desc
            .controls
            .test_pattern
            .clone()
            .ok_or(SensorError::NoRegister("test pattern"))?;
        let value = *tp
            .patterns
            .get(name)
            .ok_or_else(|| SensorError::UnknownTestPattern(name.into()))?;
        self.write_field(&tp.register, value)
    }

    /// Write values due before streaming, then the stream-on sequence. Frame numbering starts
    /// at 0.
    pub fn start_streaming(&mut self) -> Result<()> {
        self.require(
            self.state == DriverState::Powered && self.mode.is_some(),
            "start_streaming: needs power and a mode",
        )?;
        let batch = self
            .scheduler
            .as_mut()
            .map(ControlScheduler::issue_now)
            .unwrap_or_default();
        self.write_controls(&batch.controls, false)?;
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.stream_on)?;
        self.state = DriverState::Streaming;
        Ok(())
    }

    /// Run the stream-off sequence. Values still pending are written, and the schedule
    /// restarts at frame 0 for the next start.
    pub fn stop_streaming(&mut self) -> Result<()> {
        self.require(
            self.state == DriverState::Streaming,
            "stop_streaming: not streaming",
        )?;
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.stream_off)?;
        self.state = DriverState::Powered;
        if let Some(old) = self.scheduler.take() {
            let latest = old.predicted(u64::MAX);
            self.write_controls(&latest, false)?;
            let ctl = &self.desc.controls;
            self.scheduler = Some(
                ControlScheduler::new(ctl.delays, latest).with_exposure_limit(ExposureLimit {
                    min: ctl.exposure.min,
                    margin: ctl.exposure.margin,
                    fraction_bits: ctl.exposure.fraction_bits,
                }),
            );
        }
        Ok(())
    }

    /// Convert typed controls to codes for `frame`.
    pub fn codes_for(&self, frame: u64, req: &ControlRequest) -> Result<ControlSet> {
        let mode = self.mode.as_ref().ok_or(SensorError::State("no mode"))?;
        let sched = self
            .scheduler
            .as_ref()
            .ok_or(SensorError::State("no mode"))?;
        let t = &mode.timing;
        let mut set = ControlSet::new();
        if let Some(d) = req.frame_duration {
            set.set(Control::FrameLength, t.frame_length_for_duration(d).lines);
        }
        if let Some(d) = req.exposure {
            let fl = set
                .get(Control::FrameLength)
                .or_else(|| sched.predicted(frame).get(Control::FrameLength))
                .unwrap_or_else(|| t.frame_length_default());
            set.set(Control::Exposure, t.exposure(d, fl).code);
        }
        if let Some(g) = req.gain {
            let ctl = &self.desc.controls;
            let s = split_gain(&ctl.analog_gain, ctl.digital_gain.as_ref(), g);
            set.set(Control::AnalogGain, s.analog.code);
            if let Some(d) = s.digital {
                set.set(Control::DigitalGain, d.code);
            }
        }
        Ok(set)
    }

    /// Ask for typed values from frame `frame`. Returns where each lands.
    pub fn request(&mut self, frame: u64, req: &ControlRequest) -> Result<Vec<Landing>> {
        let set = self.codes_for(frame, req)?;
        self.request_codes(frame, &set)
    }

    /// Ask for raw codes from frame `frame`.
    pub fn request_codes(&mut self, frame: u64, set: &ControlSet) -> Result<Vec<Landing>> {
        let sched = self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("request: no mode"))?;
        Ok(sched.request(frame, set))
    }

    /// Frame `seq` started: write what is due (inside group hold) and return it.
    pub fn frame_start(&mut self, seq: u64) -> Result<IssueBatch> {
        self.require(
            self.state == DriverState::Streaming,
            "frame_start: not streaming",
        )?;
        let batch = self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("no mode"))?
            .frame_start(seq);
        self.write_controls(&batch.controls, true)?;
        Ok(batch)
    }

    /// Write what is due now, without waiting for the next frame start. Until the first
    /// [`Self::frame_start`] after starting the stream, the scheduler still counts values as
    /// landing on frame 0; call `frame_start(0)` first when frame 0 may already be exposing.
    pub fn issue_now(&mut self) -> Result<IssueBatch> {
        let batch = self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("no mode"))?
            .issue_now();
        let hold = self.state == DriverState::Streaming;
        self.write_controls(&batch.controls, hold)?;
        Ok(batch)
    }

    /// Record codes read back for a frame (e.g. from embedded data).
    pub fn report(&mut self, frame: u64, codes: &ControlSet) -> Result<Vec<Mismatch>> {
        Ok(self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("no mode"))?
            .report(frame, codes))
    }

    /// The typed values that produced a frame.
    pub fn applied(&self, frame: u64) -> Option<AppliedControls> {
        let mode = self.mode.as_ref()?;
        let codes = self.scheduler.as_ref()?.applied(frame);
        let t = &mode.timing;
        let ctl = &self.desc.controls;
        let fl = codes
            .values
            .get(Control::FrameLength)
            .unwrap_or_else(|| t.frame_length_default());
        let lines = t.exposure_code_to_lines(codes.values.get(Control::Exposure).unwrap_or(0));
        let ag = codes
            .values
            .get(Control::AnalogGain)
            .map_or(1.0, |c| ctl.analog_gain.gain_for_code(c));
        let dg = match (&ctl.digital_gain, codes.values.get(Control::DigitalGain)) {
            (Some(g), Some(c)) => g.gain_for_code(c),
            _ => 1.0,
        };
        Some(AppliedControls {
            frame,
            exposure: t.lines_to_duration(lines),
            exposure_lines: lines,
            analog_gain: ag,
            digital_gain: dg,
            frame_length: fl,
            frame_duration: t.frame_duration(fl),
            codes,
        })
    }
}
