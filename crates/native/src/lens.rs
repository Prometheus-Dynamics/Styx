//! The focus lens of a camera module: found next to the sensor, moved frame-exactly, and the
//! position each frame saw reported with it.
//!
//! * A kernel lens driver (`dw9807-vcm`, `ak7375`, `dw9714`): the `MEDIA_ENT_F_LENS` entity
//!   linked to the sensor by an ancillary link; moves are `V4L2_CID_FOCUS_ABSOLUTE` on its
//!   subdevice ([`find_kernel_lens`]). The sensor's data file can add a `[lens]` section
//!   (settle time, dioptre map).
//! * A VCM Styx drives over I²C: the sensor description's `[lens]` with `i2c = { address,
//!   chip }` (`styx_sensor::lens`), written on the sensor's bus.
//!
//! Moves follow the control schedule's frame starts: a position for frame `F` is written at
//! the start of `F - delay` ([`LensSchedule`]), and every frame reports the position predicted
//! for its exposure from the moves and the lens's settle time (`FrameControls::lens`). Phase
//! detection data from the embedded data (IMX708) is decoded per frame
//! ([`ControlHandle::pdaf`]).

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use styx_kernel::bus::i2c::{AddrWidth, I2cDevice, Message};
use styx_kernel::media::{EntityFunction, LinkType, Topology};
use styx_kernel::subdev::Subdev;
use styx_kernel::v4l2::{ControlValue, Controls, cid};
use styx_sensor::lens::imx708_pdaf;
use styx_sensor::{LensDescription, LensFrame, LensSchedule, RegisterBus, SensorPins, VcmFormat};

use crate::control::{ControlHandle, FrameControls, SensorControl, lock};
use crate::error::{NativeError, Result};

/// Where a camera's lens is driven from.
#[derive(Clone, Debug, PartialEq)]
pub enum LensKind {
    /// A kernel lens driver's subdevice.
    Kernel {
        /// The lens subdevice node.
        subdev: PathBuf,
        /// Its media entity name (`dw9807 10-000c`).
        entity: String,
    },
    /// A VCM on I²C.
    I2c {
        /// Bus number.
        bus: u32,
        /// 7-bit address.
        address: u16,
    },
}

/// A camera's focus lens.
#[derive(Clone, Debug, PartialEq)]
pub struct LensInfo {
    /// How it is driven.
    pub kind: LensKind,
    /// What is known of it (range, settle time, map).
    pub description: LensDescription,
}

impl LensInfo {
    /// Lowest and highest driver position.
    pub fn range(&self) -> [i32; 2] {
        self.description.range()
    }
}

/// The lens linked to sensor entity `sensor` by an ancillary link, with its control range
/// (read from its subdevice) and `data` when it applies to the lens's driver.
pub fn find_kernel_lens(
    topology: &Topology,
    sensor: u32,
    data: Option<&LensDescription>,
) -> Option<LensInfo> {
    let lens = topology
        .links
        .iter()
        .filter(|l| l.flags.link_type() == LinkType::Ancillary)
        .filter_map(|l| {
            let other = if l.source_id == sensor {
                l.sink_id
            } else if l.sink_id == sensor {
                l.source_id
            } else {
                return None;
            };
            topology
                .entity(other)
                .filter(|e| e.function == EntityFunction::LENS)
        })
        .next()?;
    let subdev = topology.devnode_path(lens.id)?;
    let range = Subdev::open_read_only(&subdev)
        .ok()
        .and_then(|sd| sd.query_control(cid::FOCUS_ABSOLUTE).ok())
        .map(|c| [c.minimum as i32, c.maximum as i32]);
    let mut description = data
        .filter(|d| d.matches(&lens.name))
        .cloned()
        .unwrap_or_else(|| LensDescription::generic(range.unwrap_or([0, 1023])));
    if let Some(r) = range {
        description.range = Some(r);
    }
    description.i2c = None;
    Some(LensInfo {
        kind: LensKind::Kernel {
            subdev,
            entity: lens.name.clone(),
        },
        description,
    })
}

/// The lens of a sensor Styx drives itself: its description's `[lens]` with `i2c`, on the
/// sensor's bus unless it names another.
pub fn i2c_lens(
    description: Option<&LensDescription>,
    sensor_bus: Option<u32>,
) -> Option<LensInfo> {
    let d = description?;
    let i2c = d.i2c.as_ref()?;
    Some(LensInfo {
        kind: LensKind::I2c {
            bus: i2c.bus.or(sensor_bus)?,
            address: i2c.address,
        },
        description: d.clone(),
    })
}

/// Moves a lens.
pub trait LensActuator: Send {
    /// Powers the actuator up or down.
    fn power(&mut self, on: bool) -> io::Result<()>;
    /// Moves to `position` (driver units).
    fn move_to(&mut self, position: i32) -> io::Result<()>;
}

/// A kernel lens driver: `FOCUS_ABSOLUTE` on its subdevice (the driver powers it with its
/// runtime PM while the subdevice is open).
#[derive(Debug)]
pub struct KernelLens(Subdev);

impl LensActuator for KernelLens {
    fn power(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    fn move_to(&mut self, position: i32) -> io::Result<()> {
        Ok(self
            .0
            .set_control(cid::FOCUS_ABSOLUTE, ControlValue::Integer(position))?)
    }
}

/// A VCM on I²C, in its chip's command format.
#[derive(Debug)]
pub struct I2cVcm {
    dev: I2cDevice,
    format: VcmFormat,
}

impl I2cVcm {
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.dev.transfer(&mut [Message::Write(bytes)])
    }
}

impl LensActuator for I2cVcm {
    fn power(&mut self, on: bool) -> io::Result<()> {
        let writes = if on {
            &self.format.power_up
        } else {
            &self.format.power_down
        };
        for w in writes.iter().filter(|w| !w.is_empty()) {
            self.write(w)?;
        }
        if on && self.format.power_up_us > 0 {
            std::thread::sleep(Duration::from_micros(u64::from(self.format.power_up_us)));
        }
        Ok(())
    }

    fn move_to(&mut self, position: i32) -> io::Result<()> {
        self.write(&self.format.encode(position))
    }
}

/// Records moves, for tests.
#[derive(Debug, Default, Clone)]
pub struct MockLens {
    /// Every move, in order.
    pub moves: Arc<std::sync::Mutex<Vec<i32>>>,
}

impl LensActuator for MockLens {
    fn power(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    fn move_to(&mut self, position: i32) -> io::Result<()> {
        lock(&self.moves).push(position);
        Ok(())
    }
}

/// Opens the actuator of a lens.
pub fn open_actuator(info: &LensInfo) -> Result<Box<dyn LensActuator>> {
    match &info.kind {
        LensKind::Kernel { subdev, .. } => {
            Ok(Box::new(KernelLens(Subdev::open(subdev).map_err(|e| {
                NativeError::InvalidConfig(format!("lens: {e}"))
            })?)))
        }
        LensKind::I2c { bus, address } => {
            let i2c = info
                .description
                .i2c
                .as_ref()
                .ok_or_else(|| NativeError::InvalidConfig("lens without i2c".into()))?;
            let dev = I2cDevice::open(*bus, *address, AddrWidth::Bits8).map_err(|e| {
                NativeError::InvalidConfig(format!("lens I2C {bus}-{address:04x}: {e}"))
            })?;
            Ok(Box::new(I2cVcm {
                dev,
                format: i2c.format(),
            }))
        }
    }
}

/// The lens side of a camera's controls.
pub struct LensControl {
    actuator: Box<dyn LensActuator>,
    schedule: LensSchedule,
    range: [i32; 2],
    /// Frame starts seen (sequence, `CLOCK_MONOTONIC`), the latest last.
    starts: VecDeque<(u64, Duration)>,
    powered: bool,
}

impl std::fmt::Debug for LensControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LensControl")
            .field("range", &self.range)
            .field("target", &self.schedule.target())
            .finish_non_exhaustive()
    }
}

impl LensControl {
    /// A lens driven by `actuator`, resting at `rest` (its default position, else the low
    /// end of its range).
    pub fn new(actuator: Box<dyn LensActuator>, description: &LensDescription) -> Self {
        let range = description.range();
        let rest = description.default_position.unwrap_or(range[0]);
        Self {
            actuator,
            schedule: LensSchedule::new(description.motion(), description.delay, rest),
            range,
            starts: VecDeque::new(),
            powered: false,
        }
    }

    /// Driver position range.
    pub fn range(&self) -> [i32; 2] {
        self.range
    }

    /// Frames from a write to the frame it is for.
    pub fn delay(&self) -> u32 {
        self.schedule.delay()
    }

    fn write(&mut self, position: i32, at: Duration) -> Result<()> {
        if !self.powered {
            self.actuator
                .power(true)
                .map_err(|e| NativeError::InvalidConfig(format!("lens power: {e}")))?;
            self.powered = true;
        }
        let p = position.clamp(self.range[0], self.range[1]);
        self.actuator
            .move_to(p)
            .map_err(|e| NativeError::InvalidConfig(format!("lens move: {e}")))?;
        self.schedule.written(at, p);
        Ok(())
    }

    /// Moves for frame `frame` (written at once when due, else at its frame start);
    /// `current` is the latest frame start (`None` before streaming: at once).
    pub fn request_at(&mut self, frame: u64, position: i32, current: Option<u64>) -> Result<()> {
        let now = styx_kernel::monotonic_now();
        match self.schedule.request(frame, position, current) {
            None => self.write(position, now),
            Some(_) if current.is_none() => self.write(position, now),
            Some(_) => Ok(()),
        }
    }

    /// Frame `seq` started at `at`: writes what is due.
    pub fn frame_start(&mut self, seq: u64, at: Duration) -> Result<()> {
        self.starts.push_back((seq, at));
        while self.starts.len() > 32 {
            self.starts.pop_front();
        }
        match self.schedule.due(seq) {
            Some(p) => self.write(p, at),
            None => Ok(()),
        }
    }

    /// Where the lens was for frame `seq` given its exposure and readout time.
    pub fn frame(&self, seq: u64, exposure: Duration, readout: Duration) -> Option<LensFrame> {
        let (_, at) = self.starts.iter().rev().find(|(s, _)| *s == seq)?;
        // Rows end their exposure from the frame start to the end of the readout.
        Some(
            self.schedule
                .frame(at.saturating_sub(exposure), *at + readout),
        )
    }

    /// Puts the actuator in standby.
    pub fn power_down(&mut self) {
        if self.powered {
            let _ = self.actuator.power(false);
            self.powered = false;
        }
    }

    /// Forgets frame starts (a new stream numbers frames from 0 again).
    pub fn restart(&mut self) {
        self.starts.clear();
    }
}

/// Phase detection data decoded from the embedded data: which layout, and the last frames'.
#[derive(Debug, Default)]
pub struct PdafFrames {
    /// The sensor's layout (`imx708`).
    pub format: Option<String>,
    frames: VecDeque<(u64, Arc<[imx708_pdaf::Cell]>)>,
}

impl PdafFrames {
    /// Decodes frame `seq`'s embedded data (mode `width`, `bits` per pixel).
    pub fn decode(&mut self, seq: u64, data: &[u8], width: u32, bits: u32) {
        if self.format.as_deref() != Some("imx708") {
            return;
        }
        // As libcamera: the PDAF line starts two mode lines into the buffer.
        let offset = 2 * (width as usize * bits as usize / 8);
        if let Some(cells) = data.get(offset..).and_then(|l| imx708_pdaf::parse(l, bits)) {
            self.frames.push_back((seq, cells.into()));
            while self.frames.len() > 8 {
                self.frames.pop_front();
            }
        }
    }

    /// Frame `seq`'s cells (16×12, row-major).
    pub fn get(&self, seq: u64) -> Option<Arc<[imx708_pdaf::Cell]>> {
        self.frames
            .iter()
            .rev()
            .find(|(s, _)| *s == seq)
            .map(|(_, c)| Arc::clone(c))
    }
}

/// Opens the lens of `info` (if any) for a camera's control, and the phase detection layout
/// its data names. A lens that cannot be opened leaves the camera without one.
pub(crate) fn attach<B: RegisterBus, P: SensorPins>(
    info: &crate::CameraInfo,
    control: &std::sync::Mutex<SensorControl<B, P>>,
) {
    let mut c = lock(control);
    if let Some(l) = &info.lens
        && let Ok(actuator) = open_actuator(l)
    {
        c.set_lens(Some(LensControl::new(actuator, &l.description)));
    }
    let pdaf = info
        .kernel
        .as_ref()
        .and_then(|k| k.data.as_ref())
        .and_then(|d| d.pdaf.clone());
    c.set_pdaf_format(pdaf);
}

impl<B: RegisterBus, P: SensorPins> SensorControl<B, P> {
    /// Gives the camera a lens.
    pub fn set_lens(&mut self, lens: Option<LensControl>) {
        self.lens = lens;
    }

    /// Phase detection data in this layout (`imx708`) is decoded from the embedded data.
    pub fn set_pdaf_format(&mut self, format: Option<String>) {
        self.pdaf.format = format;
    }

    pub(crate) fn lens_frame_start(&mut self, seq: u64, at: Option<Duration>) -> Result<()> {
        if seq == 0
            && let Some(l) = &mut self.lens
        {
            l.restart();
        }
        match &mut self.lens {
            Some(l) => l.frame_start(seq, at.unwrap_or_else(styx_kernel::monotonic_now)),
            None => Ok(()),
        }
    }

    /// Fills in where the lens was for the frame `f` describes.
    pub(crate) fn with_lens(&self, seq: u64, mut f: FrameControls) -> FrameControls {
        if let (Some(l), Some(t)) = (&self.lens, self.timing()) {
            let line = f64::from(t.width + t.hblank) / t.pixel_rate.max(1) as f64;
            let readout = Duration::from_secs_f64(line * f64::from(t.height));
            f.lens = l.frame(seq, f.exposure, readout);
        }
        f
    }

    pub(crate) fn decode_pdaf(&mut self, seq: u64, data: &[u8]) {
        if self.pdaf.format.is_none() {
            return;
        }
        if let Some(m) = self.driver().mode() {
            let bits = u32::from(m.code.bit_depth().unwrap_or(10));
            let width = m.timing.width;
            self.pdaf.decode(seq, data, width, bits);
        }
    }
}

impl<B: RegisterBus, P: SensorPins> ControlHandle<B, P> {
    /// Whether the camera has a focus lens.
    pub fn has_lens(&self) -> bool {
        lock(self.shared()).lens.is_some()
    }

    /// The lens's driver position range and its delay in frames.
    pub fn lens_range(&self) -> Option<([i32; 2], u32)> {
        lock(self.shared())
            .lens
            .as_ref()
            .map(|l| (l.range(), l.delay()))
    }

    /// Moves the lens to `position` (driver units) for frame `frame` on: written at the
    /// start of `frame - delay`, or at once when that has passed (or before streaming).
    pub fn request_lens_at(&self, frame: u64, position: i32) -> Result<()> {
        let mut c = lock(self.shared());
        let current = c.current_frame().filter(|_| c.is_streaming());
        match &mut c.lens {
            Some(l) => l.request_at(frame, position, current),
            None => Err(NativeError::InvalidConfig("the camera has no lens".into())),
        }
    }

    /// Moves the lens to `position` as soon as possible.
    pub fn set_lens_position(&self, position: i32) -> Result<()> {
        let mut c = lock(self.shared());
        match &mut c.lens {
            Some(l) => l.request_at(0, position, None),
            None => Err(NativeError::InvalidConfig("the camera has no lens".into())),
        }
    }

    /// Frame `seq`'s phase detection cells, when the sensor sends them.
    pub fn pdaf(&self, seq: u64) -> Option<Arc<[imx708_pdaf::Cell]>> {
        lock(self.shared()).pdaf.get(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use styx_sensor::LensMotion;

    #[test]
    fn moves_are_written_at_their_frame_start_and_frames_report_them() {
        let lens = MockLens::default();
        let moves = Arc::clone(&lens.moves);
        let mut d = LensDescription::generic([0, 1023]);
        d.settle_us = 10_000;
        let mut c = LensControl::new(Box::new(lens), &d);
        let ms = Duration::from_millis;
        for seq in 0..3 {
            c.frame_start(seq, ms(33 * seq)).unwrap();
        }
        // For frame 6: written at the start of frame 4.
        c.request_at(6, 500, Some(2)).unwrap();
        c.frame_start(3, ms(99)).unwrap();
        assert!(lock(&moves).is_empty());
        c.frame_start(4, ms(132)).unwrap();
        assert_eq!(*lock(&moves), [500]);
        for seq in 5..7 {
            c.frame_start(seq, ms(33 * seq)).unwrap();
        }
        // Frame 3 saw the rest position; frame 4's rows were read out while the lens moved
        // (written at its start); frame 5's exposure began 1 ms after the move; frame 6 saw
        // the new position.
        let f = c.frame(3, ms(10), ms(5)).unwrap();
        assert_eq!((f.position, f.settled), (0.0, true));
        assert!(!c.frame(4, ms(10), ms(5)).unwrap().settled);
        let f = c.frame(5, ms(32), ms(5)).unwrap();
        assert!(!f.settled);
        let f = c.frame(6, ms(10), ms(5)).unwrap();
        assert_eq!((f.position, f.settled, f.target), (500.0, true, 500));
        // Late requests are written at once; positions are clamped to the range.
        c.request_at(5, 2000, Some(6)).unwrap();
        assert_eq!(*lock(&moves), [500, 1023]);
        let _ = LensMotion { settle: ms(1) };
    }

    #[test]
    fn pdaf_frames_decode_at_the_third_line() {
        let mut p = PdafFrames {
            format: Some("imx708".into()),
            ..Default::default()
        };
        let width = 1536;
        let mut data = vec![0u8; 3 * width * 10 / 8];
        let at = 2 * width * 10 / 8 + 2 * 5;
        data[at] = 0x10;
        p.decode(7, &data, width as u32, 10);
        let cells = p.get(7).unwrap();
        assert_eq!(cells.len(), 192);
        assert_eq!(cells[0].conf, 0x80);
        assert!(p.get(6).is_none());
    }
}
