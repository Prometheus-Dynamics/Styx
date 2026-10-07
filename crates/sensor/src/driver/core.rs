//! The driver's logic, written once over the async traits. [`SensorDriver`] runs it over
//! blocking implementations through [`styx_hal::Blocking`] (every await is ready at once, so
//! one poll completes it); [`AsyncSensorDriver`] awaits it.
//!
//! [`SensorDriver`]: super::SensorDriver
//! [`AsyncSensorDriver`]: super::AsyncSensorDriver

use crate::Arc;
use alloc::{borrow::ToOwned, format, string::String, vec::Vec};

use styx_hal::AsyncSensorPins;

use super::{ActiveMode, AppliedControls, ControlRequest, DriverState};
use crate::bus::AsyncDriverBus;
use crate::bus_error::{BusError, BusErrorKind};
use crate::desc::{Backend, Field, Flip, RegWrite, SensorDescription, Step};
use crate::error::{Result, SensorError};
use crate::fallback::{KernelControl, KernelControls, kernel_controls};
use crate::gain::split_gain;
use crate::mbus::ColorFilter;
use crate::schedule::{
    Applied, Control, ControlScheduler, ControlSet, ExposureLimit, IssueBatch, Landings, Mismatches,
};
use crate::timing::Timing;

/// Register writes of a sequence sent per `write_sequence` call (consecutive writes between
/// pin steps; longer runs go out in several calls, in order).
const BATCH: usize = 64;

#[derive(Debug)]
pub(crate) struct DriverCore<B, P> {
    pub(crate) desc: Arc<SensorDescription>,
    pub(crate) bus: B,
    pub(crate) pins: P,
    pub(crate) state: DriverState,
    pub(crate) mode: Option<ActiveMode>,
    flips: (bool, bool),
    pub(crate) scheduler: Option<ControlScheduler>,
}

fn new_scheduler(desc: &SensorDescription, initial: ControlSet) -> ControlScheduler {
    let ctl = &desc.controls;
    ControlScheduler::new(ctl.delays, initial).with_exposure_limit(ExposureLimit {
        min: ctl.exposure.min,
        margin: ctl.exposure.margin,
        fraction_bits: ctl.exposure.fraction_bits,
    })
}

fn require(ok: bool, msg: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(SensorError::State(msg))
    }
}

/// The pure part: state, mode, schedule, conversions.
impl<B, P> DriverCore<B, P> {
    pub(crate) fn new(desc: Arc<SensorDescription>, bus: B, pins: P) -> Self {
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

    pub(crate) fn is_kernel(&self) -> bool {
        self.desc.sensor.backend == Backend::Kernel
    }

    pub(crate) fn set_description(&mut self, desc: Arc<SensorDescription>) -> Result<()> {
        require(
            self.state != DriverState::Streaming,
            "set_description: streaming",
        )?;
        self.desc = desc;
        self.mode = None;
        self.scheduler = None;
        Ok(())
    }

    pub(crate) fn color_filter(&self) -> ColorFilter {
        self.desc.color_filter(self.flips.0, self.flips.1)
    }

    pub(crate) fn codes_for(&self, frame: u64, req: &ControlRequest) -> Result<ControlSet> {
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

    pub(crate) fn request_codes(&mut self, frame: u64, set: &ControlSet) -> Result<Landings> {
        let sched = self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("request: no mode"))?;
        Ok(sched.request(frame, set))
    }

    pub(crate) fn report(&mut self, frame: u64, codes: &ControlSet) -> Result<Mismatches> {
        Ok(self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("no mode"))?
            .report(frame, codes))
    }

    pub(crate) fn applied(&self, frame: u64) -> Option<AppliedControls> {
        let mode = self.mode.as_ref()?;
        let codes: Applied = self.scheduler.as_ref()?.applied(frame);
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

/// The part that talks to the sensor.
impl<B: AsyncDriverBus, P: AsyncSensorPins> DriverCore<B, P> {
    async fn run(&mut self, steps: &[Step]) -> Result<()> {
        let mut batch = [RegWrite::byte(0, 0); BATCH];
        let mut n = 0;
        for step in steps {
            if let Step::Write(w) = step {
                if n == BATCH {
                    self.write_batch(&batch[..n]).await?;
                    n = 0;
                }
                batch[n] = *w;
                n += 1;
                continue;
            }
            self.write_batch(&batch[..n]).await?;
            n = 0;
            self.pin_step(step).await?;
        }
        self.write_batch(&batch[..n]).await
    }

    async fn pin_step(&mut self, step: &Step) -> Result<()> {
        let (what, role, optional, res) = match step {
            Step::Write(w) => return self.write_batch(core::slice::from_ref(w)).await,
            Step::Delay(d) => {
                styx_hal::wait_async(&mut self.pins, *d).await;
                return Ok(());
            }
            Step::Gpio {
                role,
                value,
                optional,
            } => (
                "gpio",
                role,
                *optional,
                self.pins.set_gpio(role, *value).await,
            ),
            Step::Clock { role, on, optional } => {
                let rate = on.then(|| self.desc.sensor.clocks.get(role).copied().unwrap_or(0));
                (
                    "clock",
                    role,
                    *optional,
                    self.pins.set_clock(role, rate).await,
                )
            }
            Step::Supply { role, on, optional } => (
                "supply",
                role,
                *optional,
                self.pins.set_supply(role, *on).await,
            ),
        };
        match res.map_err(|e| BusError::from_hal(&e)) {
            Err(e) if optional && e.kind() == BusErrorKind::NotFound => Ok(()),
            Err(source) => Err(SensorError::Pins {
                what,
                role: role.clone(),
                source,
            }),
            Ok(()) => Ok(()),
        }
    }

    async fn write_batch(&mut self, batch: &[RegWrite]) -> Result<()> {
        let Some(first) = batch.first() else {
            return Ok(());
        };
        let address = first.address;
        self.bus
            .write_sequence(batch)
            .await
            .map_err(|e| SensorError::Bus {
                op: "write sequence at",
                address,
                source: BusError::from_register(e),
            })
    }

    async fn write_field(&mut self, field: &Field, value: u32) -> Result<()> {
        let current = if field.read_modify_write {
            Some(self.read(field.address, field.bytes).await?)
        } else {
            None
        };
        let reg = field.encode(value, current);
        self.bus
            .write(field.address, field.bytes, reg)
            .await
            .map_err(|e| SensorError::Bus {
                op: "write",
                address: field.address,
                source: BusError::from_register(e),
            })
    }

    pub(crate) async fn read(&mut self, address: u16, bytes: u8) -> Result<u32> {
        self.bus
            .read(address, bytes)
            .await
            .map_err(|e| SensorError::Bus {
                op: "read",
                address,
                source: BusError::from_register(e),
            })
    }

    pub(crate) async fn power_up(&mut self) -> Result<()> {
        require(self.state == DriverState::Off, "power_up: already powered")?;
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.power_up).await?;
        self.state = DriverState::Powered;
        Ok(())
    }

    pub(crate) async fn power_down(&mut self) -> Result<()> {
        if self.state == DriverState::Streaming {
            self.stop_streaming().await?;
        }
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.power_down).await?;
        self.state = DriverState::Off;
        self.mode = None;
        self.scheduler = None;
        Ok(())
    }

    pub(crate) async fn force_power_down(&mut self) -> Result<()> {
        let desc = Arc::clone(&self.desc);
        let mut first = Ok(());
        for step in desc.sequences.power_down.iter() {
            if matches!(step, Step::Write(_)) {
                continue;
            }
            let r = self.pin_step(step).await;
            if first.is_ok() {
                first = r;
            }
        }
        self.state = DriverState::Off;
        self.mode = None;
        self.scheduler = None;
        first
    }

    pub(crate) async fn verify_chip_id(&mut self) -> Result<u32> {
        require(
            self.state != DriverState::Off,
            "verify_chip_id: not powered",
        )?;
        let desc = Arc::clone(&self.desc);
        let Some(id) = &desc.sensor.chip_id else {
            return Ok(0);
        };
        let found = self.read(id.address, id.bytes).await?;
        if id.values.contains(&found) {
            Ok(found)
        } else {
            Err(SensorError::ChipId {
                expected: id.values.clone(),
                found,
            })
        }
    }

    pub(crate) async fn init(&mut self) -> Result<()> {
        require(
            self.state == DriverState::Powered,
            "init: not powered or streaming",
        )?;
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.init).await
    }

    pub(crate) async fn set_mode(&mut self, mode: &str, format: &str) -> Result<()> {
        require(
            self.state == DriverState::Powered,
            "set_mode: not powered or streaming",
        )?;
        let desc = Arc::clone(&self.desc);
        let m = desc.mode(mode)?;
        let f = desc.format_for(m, format)?;
        let timing = Timing::for_mode(m, f, &desc.controls.exposure)
            .with_extra_lines(desc.controls.frame_length_extra_lines);
        let ctl = &desc.controls;
        if !self.is_kernel() {
            ctl.frame_length
                .ok_or(SensorError::NoRegister("frame length"))?;
        }
        self.run(&f.registers).await?;
        self.run(&m.registers).await?;
        if let Some(ll) = ctl.line_length {
            self.write_field(
                &ll.register,
                timing.line_length() / ll.pixels_per_unit.max(1),
            )
            .await?;
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
        self.write_controls_for(&initial, false, timing.height)
            .await?;
        let (h, v) = self.flips;
        self.write_flips(h, v).await?;
        self.scheduler = Some(new_scheduler(&desc, initial));
        self.mode = Some(ActiveMode {
            mode: m.name.clone(),
            format: format.to_owned(),
            code: f.code,
            timing,
        });
        Ok(())
    }

    pub(crate) async fn set_hblank(&mut self, hblank: u32) -> Result<Timing> {
        require(
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
        self.write_field(&ll.register, t.line_length() / ll.pixels_per_unit.max(1))
            .await?;
        Ok(t)
    }

    /// Write control codes (frame length first), inside group hold when asked and available.
    async fn write_controls(&mut self, set: &ControlSet, hold: bool) -> Result<()> {
        let height = self.mode.as_ref().map_or(0, |m| m.timing.height);
        self.write_controls_for(set, hold, height).await
    }

    /// [`Self::write_controls`] for a mode of `height` lines (kernel-driven sensors set the
    /// frame length as `VBLANK = frame length - height`).
    async fn write_controls_for(
        &mut self,
        set: &ControlSet,
        hold: bool,
        height: u32,
    ) -> Result<()> {
        if set.is_empty() {
            return Ok(());
        }
        if self.is_kernel() {
            return self.write_kernel_controls(set, height).await;
        }
        let desc = Arc::clone(&self.desc);
        let ctl = &desc.controls;
        let gh = ctl.group_hold.as_ref().filter(|_| hold);
        if let Some(gh) = gh {
            self.run(&gh.start).await?;
        }
        for (c, v) in set.iter() {
            let field = match c {
                Control::FrameLength => ctl
                    .frame_length
                    .ok_or(SensorError::NoRegister("frame length"))?,
                Control::Exposure => {
                    let f = ctl
                        .exposure
                        .register
                        .ok_or(SensorError::NoRegister("exposure"))?;
                    let fb = ctl.exposure.fraction_bits;
                    Field {
                        shift: f.shift - fb,
                        bits: Some(f.bits() + fb),
                        ..f
                    }
                }
                Control::AnalogGain => ctl
                    .analog_gain
                    .register
                    .ok_or(SensorError::NoRegister("analogue gain"))?,
                Control::DigitalGain => {
                    let g = ctl.digital_gain.as_ref().and_then(|g| g.register);
                    g.ok_or(SensorError::NoRegister("digital gain"))?
                }
            };
            self.write_field(&field, v).await?;
        }
        if let Some(gh) = gh {
            self.run(&gh.end).await?;
            self.run(&gh.launch).await?;
        }
        Ok(())
    }

    /// Kernel-driven sensors: the codes as V4L2 controls. `VBLANK` goes first in a call of its
    /// own: the driver widens the exposure range when the frame length grows, and an exposure
    /// set in the same call would be clamped to the old range first (libcamera writes it with
    /// priority for the same reason).
    async fn write_kernel_controls(&mut self, set: &ControlSet, height: u32) -> Result<()> {
        let fb = self.desc.controls.exposure.fraction_bits;
        let all = kernel_controls(set, height, fb);
        let mut vblank = all;
        vblank.retain(|(c, _)| *c == KernelControl::Vblank);
        let mut rest = all;
        rest.retain(|(c, _)| *c != KernelControl::Vblank);
        self.set_kernel(&vblank).await?;
        self.set_kernel(&rest).await
    }

    async fn set_kernel(&mut self, batch: &[(KernelControl, i64)]) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        self.bus
            .set_controls(batch)
            .await
            .map_err(|source| SensorError::Controls {
                controls: batch
                    .iter()
                    .map(|(c, v)| format!("{c:?}={v}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                source,
            })
    }

    async fn write_flip(&mut self, flip: Option<Flip>, on: bool) -> Result<()> {
        let Some(f) = flip else { return Ok(()) };
        let current = self.read(f.address, 1).await?;
        let mask = u32::from(f.mask);
        let value = if on { current | mask } else { current & !mask };
        self.bus
            .write(f.address, 1, value)
            .await
            .map_err(|e| SensorError::Bus {
                op: "write",
                address: f.address,
                source: BusError::from_register(e),
            })
    }

    async fn write_flips(&mut self, h: bool, v: bool) -> Result<()> {
        if self.is_kernel() {
            let ctl = &self.desc.controls;
            let set: KernelControls = [
                (ctl.hflip.is_some(), KernelControl::HFlip, h),
                (ctl.vflip.is_some(), KernelControl::VFlip, v),
            ]
            .into_iter()
            .filter(|(has, ..)| *has)
            .map(|(_, c, on)| (c, i64::from(on)))
            .collect();
            return self.set_kernel(&set).await;
        }
        self.write_flip(self.desc.controls.hflip, h).await?;
        self.write_flip(self.desc.controls.vflip, v).await
    }

    pub(crate) async fn set_flips(&mut self, hflip: bool, vflip: bool) -> Result<()> {
        require(self.state != DriverState::Off, "set_flips: not powered")?;
        let changes = self.desc.color_filter(hflip, vflip) != self.color_filter();
        require(
            !(changes && self.state == DriverState::Streaming),
            "set_flips: would change the Bayer order while streaming",
        )?;
        self.write_flips(hflip, vflip).await?;
        self.flips = (hflip, vflip);
        Ok(())
    }

    pub(crate) async fn set_test_pattern(&mut self, name: &str) -> Result<()> {
        require(
            self.state != DriverState::Off,
            "set_test_pattern: not powered",
        )?;
        let desc = Arc::clone(&self.desc);
        let tp = desc
            .controls
            .test_pattern
            .as_ref()
            .ok_or(SensorError::NoRegister("test pattern"))?;
        let value = *tp
            .patterns
            .get(name)
            .ok_or_else(|| SensorError::UnknownTestPattern(String::from(name)))?;
        if self.is_kernel() {
            return self
                .set_kernel(&[(KernelControl::TestPattern, i64::from(value))])
                .await;
        }
        self.write_field(&tp.register, value).await
    }

    pub(crate) async fn start_streaming(&mut self) -> Result<()> {
        require(
            self.state == DriverState::Powered && self.mode.is_some(),
            "start_streaming: needs power and a mode",
        )?;
        let batch = self
            .scheduler
            .as_mut()
            .map(ControlScheduler::issue_now)
            .unwrap_or_default();
        self.write_controls(&batch.controls, false).await?;
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.stream_on).await?;
        self.state = DriverState::Streaming;
        Ok(())
    }

    pub(crate) async fn stop_streaming(&mut self) -> Result<()> {
        require(
            self.state == DriverState::Streaming,
            "stop_streaming: not streaming",
        )?;
        let desc = Arc::clone(&self.desc);
        self.run(&desc.sequences.stream_off).await?;
        self.state = DriverState::Powered;
        // The old schedule's last values (not the ~9 KiB schedule) are kept across the write.
        let latest = self.scheduler.take().map(|old| old.predicted(u64::MAX));
        if let Some(latest) = latest {
            self.write_controls(&latest, false).await?;
            self.scheduler = Some(new_scheduler(&desc, latest));
        }
        Ok(())
    }

    pub(crate) async fn request_now(
        &mut self,
        frame: u64,
        req: &ControlRequest,
    ) -> Result<Landings> {
        let set = self.codes_for(frame, req)?;
        let (landings, batch) = self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("request: no mode"))?
            .request_now(frame, &set);
        let hold = self.state == DriverState::Streaming;
        self.write_controls(&batch.controls, hold).await?;
        Ok(landings)
    }

    pub(crate) async fn frame_start(&mut self, seq: u64) -> Result<IssueBatch> {
        require(
            self.state == DriverState::Streaming,
            "frame_start: not streaming",
        )?;
        let batch = self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("no mode"))?
            .frame_start(seq);
        self.write_controls(&batch.controls, true).await?;
        Ok(batch)
    }

    pub(crate) async fn issue_now(&mut self) -> Result<IssueBatch> {
        let batch = self
            .scheduler
            .as_mut()
            .ok_or(SensorError::State("no mode"))?
            .issue_now();
        let hold = self.state == DriverState::Streaming;
        self.write_controls(&batch.controls, hold).await?;
        Ok(batch)
    }
}
