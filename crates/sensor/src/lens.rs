//! Focus lenses (voice-coil motors) as data: what a lens can do, how a VCM chip is driven over
//! I²C, how long a move takes, and the frame-exact schedule of moves with the position each
//! frame saw.
//!
//! Two ways to drive a lens, like sensors:
//!
//! * a kernel lens driver (`dw9807-vcm`, `ak7375`, `dw9714`, …): a `MEDIA_ENT_F_LENS`
//!   subdevice linked to the sensor by an ancillary link, moved with `V4L2_CID_FOCUS_ABSOLUTE`;
//!   the data here adds what the driver does not report (settle time, the dioptre map);
//! * a VCM Styx drives itself over I²C ([`VcmI2c`]): which chip ([`VcmChip`], or a
//!   [`VcmFormat`] for another one) at which address. The chips' command formats and the
//!   driver are Lemnos's (`lemnos-drivers-vcm`); this module only describes the lens and
//!   schedules its moves.
//!
//! ```toml
//! [lens]                         # in a sensor description or a kernel data file
//! range = [0, 1023]              # driver positions
//! settle_us = 12000              # a move settles within this
//! delay = 2                      # frames from writing a move to the frame it is for
//! map = [0.0, 445, 15.0, 925]    # dioptres -> position (AF tuning's map wins)
//! i2c = { address = 0x0c, chip = "dw9807" }   # only for a VCM Styx drives
//! ```
//!
//! VCMs report no position: [`LensSchedule`] predicts each frame's from the moves written and
//! [`LensMotion`] (a first-order approach that is complete after `settle`).

use crate::fixed::FixedVec;
use crate::frame_map::FrameMap;
use alloc::{format, string::String, vec::Vec};
use core::time::Duration;
use lemnos_drivers_vcm as vcm;

use serde::{Deserialize, Serialize};

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

/// A focus lens.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LensDescription {
    /// The actuator, informational (`dw9817`).
    #[serde(default)]
    pub name: Option<String>,
    /// Kernel lens driver entity names (first word) this applies to; empty: any lens linked
    /// to the sensor.
    #[serde(default)]
    pub drivers: Vec<String>,
    /// Lowest and highest driver position. Default: the kernel control's range, else the
    /// chip's.
    #[serde(default)]
    pub range: Option<[i32; 2]>,
    /// Where the lens rests without power / where to start (default: the AF tuning's default
    /// focus).
    #[serde(default)]
    pub default_position: Option<i32>,
    /// Time for a move to settle, in microseconds.
    #[serde(default = "default_settle_us")]
    pub settle_us: u32,
    /// Frames from the frame a move is written in to the first frame exposed with the lens
    /// there.
    #[serde(default = "default_delay")]
    pub delay: u32,
    /// Dioptres → position as `[d0, p0, d1, p1, …]` (empty: the AF tuning's map, else a
    /// straight line over the range).
    #[serde(default)]
    pub map: Vec<f64>,
    /// A VCM Styx drives over I²C (absent for a kernel lens driver).
    #[serde(default)]
    pub i2c: Option<VcmI2c>,
}

fn default_settle_us() -> u32 {
    12_000
}

fn default_delay() -> u32 {
    2
}

impl LensDescription {
    /// A lens known only from a kernel driver: its control's range, the defaults otherwise.
    pub fn generic(range: [i32; 2]) -> Self {
        Self {
            name: None,
            drivers: Vec::new(),
            range: Some(range),
            default_position: None,
            settle_us: default_settle_us(),
            delay: default_delay(),
            map: Vec::new(),
            i2c: None,
        }
    }

    /// The range: the description's, else the chip's, else 10 bits.
    pub fn range(&self) -> [i32; 2] {
        self.range
            .or_else(|| self.i2c.as_ref().map(|i| [0, i.max_position()]))
            .unwrap_or([0, 1023])
    }

    /// The move time model.
    pub fn motion(&self) -> LensMotion {
        LensMotion {
            settle: Duration::from_micros(u64::from(self.settle_us)),
        }
    }

    /// Whether this applies to a kernel lens entity named `entity` (first word compared).
    pub fn matches(&self, entity: &str) -> bool {
        let driver = entity.split_whitespace().next().unwrap_or("");
        self.drivers.is_empty() || self.drivers.iter().any(|d| d == driver)
    }

    /// Problems with the values.
    pub fn check(&self) -> Result<(), String> {
        let [lo, hi] = self.range();
        if lo >= hi {
            return Err(format!("lens range {lo}..{hi} is empty"));
        }
        if !self.map.len().is_multiple_of(2) || self.map.len() == 2 {
            return Err("lens map needs two or more [dioptres, position] pairs".into());
        }
        if let Some(i) = &self.i2c {
            i.check()?;
        }
        Ok(())
    }
}

/// VCM chips whose command formats Lemnos has built in (`lemnos_drivers_vcm::VcmChip`), by
/// their description name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VcmChip {
    /// Dongwoon DW9714.
    Dw9714,
    /// Dongwoon DW9807.
    Dw9807,
    /// Dongwoon DW9817 (Raspberry Pi Camera Module 3).
    Dw9817,
    /// Asahi Kasei AK7375.
    Ak7375,
    /// Another chip: [`VcmI2c::format`] describes it.
    Custom,
}

impl VcmChip {
    /// Lemnos's chip (`None` for [`VcmChip::Custom`]).
    pub fn lemnos(self) -> Option<vcm::VcmChip> {
        match self {
            VcmChip::Dw9714 => Some(vcm::VcmChip::Dw9714),
            VcmChip::Dw9807 => Some(vcm::VcmChip::Dw9807),
            VcmChip::Dw9817 => Some(vcm::VcmChip::Dw9817),
            VcmChip::Ak7375 => Some(vcm::VcmChip::Ak7375),
            VcmChip::Custom => None,
        }
    }
}

/// Most power-up or power-down writes a [`VcmFormat`] may list.
pub const MAX_VCM_WRITES: usize = 4;

/// The command format of a chip Lemnos does not have built in, as a description writes it
/// (`lemnos_drivers_vcm::VcmFormat` with owned messages): `[register?] ((position << shift) |
/// or)` big-endian in `bytes`, and the raw power-up and power-down writes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VcmFormat {
    /// The register the value goes to (none: the value is the whole message).
    #[serde(default)]
    pub register: Option<u8>,
    /// Bytes of the value (1 or 2).
    #[serde(default = "two")]
    pub bytes: u8,
    /// Bits the position is shifted left.
    #[serde(default)]
    pub shift: u8,
    /// Bits of the position.
    #[serde(default = "ten")]
    pub bits: u8,
    /// Constant bits or-ed in (mode, slew).
    #[serde(default)]
    pub or: u16,
    /// Writes (raw messages) that power the chip up, then the time to wait (at most
    /// [`MAX_VCM_WRITES`]).
    #[serde(default)]
    pub power_up: Vec<Vec<u8>>,
    /// Microseconds after power-up before the first move.
    #[serde(default)]
    pub power_up_us: u32,
    /// Writes that put the chip in standby (at most [`MAX_VCM_WRITES`]).
    #[serde(default)]
    pub power_down: Vec<Vec<u8>>,
}

fn two() -> u8 {
    2
}

fn ten() -> u8 {
    10
}

impl VcmFormat {
    /// Runs `f` with this format as Lemnos's (borrowing the messages; nothing allocated).
    pub fn with_lemnos<R>(&self, f: impl FnOnce(&vcm::VcmFormat<'_>) -> R) -> R {
        let mut up: [&[u8]; MAX_VCM_WRITES] = [&[]; MAX_VCM_WRITES];
        let mut down: [&[u8]; MAX_VCM_WRITES] = [&[]; MAX_VCM_WRITES];
        for (slot, m) in up.iter_mut().zip(&self.power_up) {
            *slot = m;
        }
        for (slot, m) in down.iter_mut().zip(&self.power_down) {
            *slot = m;
        }
        f(&vcm::VcmFormat {
            register: self.register,
            bytes: self.bytes,
            shift: self.shift,
            bits: self.bits,
            or: self.or,
            power_up: &up[..self.power_up.len().min(MAX_VCM_WRITES)],
            power_up_us: self.power_up_us,
            power_down: &down[..self.power_down.len().min(MAX_VCM_WRITES)],
        })
    }

    fn check(&self) -> Result<(), String> {
        if self.power_up.len() > MAX_VCM_WRITES || self.power_down.len() > MAX_VCM_WRITES {
            return Err(format!(
                "VCM format: at most {MAX_VCM_WRITES} power-up and power-down writes"
            ));
        }
        self.with_lemnos(|f| f.check())
            .map_err(|e| format!("VCM format: {e}"))
    }
}

/// A VCM Styx drives over I²C (through `lemnos-drivers-vcm`).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VcmI2c {
    /// 7-bit address (0x0c for the DW9714/DW9807/DW9817 family).
    pub address: u16,
    /// The bus (default: the sensor's).
    #[serde(default)]
    pub bus: Option<u32>,
    /// The chip.
    pub chip: VcmChip,
    /// The command format of a [`VcmChip::Custom`] chip (or overrides of a known one).
    #[serde(default)]
    pub format: Option<VcmFormat>,
}

impl VcmI2c {
    /// Runs `f` with the command format: the given one, else Lemnos's for the chip (a
    /// [`VcmChip::Custom`] chip without one has the DW9807's layout, and fails
    /// [`LensDescription::check`]).
    pub fn with_format<R>(&self, f: impl FnOnce(&vcm::VcmFormat<'_>) -> R) -> R {
        match &self.format {
            Some(format) => format.with_lemnos(f),
            None => f(&self.chip.lemnos().unwrap_or(vcm::VcmChip::Dw9807).format()),
        }
    }

    /// The highest position the chip takes.
    pub fn max_position(&self) -> i32 {
        self.with_format(|f| f.max_position())
    }

    fn check(&self) -> Result<(), String> {
        if u8::try_from(self.address).map_or(true, |a| a > 0x7f) {
            return Err(format!("lens i2c address {:#x} is not 7-bit", self.address));
        }
        match &self.format {
            Some(f) => f.check(),
            None if self.chip == VcmChip::Custom => {
                Err("a custom VCM chip needs a `format`".into())
            }
            None => Ok(()),
        }
    }
}

/// How the lens moves: a first-order approach to the target, complete (within a code) after
/// `settle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LensMotion {
    /// Time to settle.
    pub settle: Duration,
}

impl LensMotion {
    /// Position `dt` after a move from `from` to `to` was written; settled when `dt` ≥
    /// `settle` (then exactly `to`).
    pub fn position(&self, from: f64, to: f64, dt: Duration) -> (f64, bool) {
        if dt >= self.settle {
            return (to, true);
        }
        let tau = (self.settle.as_secs_f64() / 5.0).max(1e-9);
        (to + (from - to) * (-dt.as_secs_f64() / tau).exp(), false)
    }
}

/// A move written: when, from where (predicted), to where.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Move {
    at: Duration,
    from: f64,
    to: f64,
}

/// Where the lens was for one frame (as `styx-algo`'s `LensState`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LensFrame {
    /// Predicted position at the middle of the exposure.
    pub position: f64,
    /// Settled at the last move's target for the whole exposure.
    pub settled: bool,
    /// The position last written.
    pub target: i32,
}

/// Frame-exact lens moves: a move for frame `F` is written at the start of frame
/// `F - delay` (or at once when that has passed), and each frame's position is predicted from
/// the moves and their times ([`LensMotion`]).
#[derive(Debug, Clone)]
pub struct LensSchedule {
    motion: LensMotion,
    delay: u32,
    /// Write frame -> position (fixed: lens moves run per frame without allocating).
    pending: FrameMap<i32, 16>,
    /// The last [`MOVES_KEPT`] moves, oldest first.
    moves: FixedVec<Move, MOVES_KEPT>,
    rest: f64,
}

/// Moves kept to predict positions from.
const MOVES_KEPT: usize = 16;

impl LensSchedule {
    /// A schedule for a lens resting at `position`.
    pub fn new(motion: LensMotion, delay: u32, position: i32) -> Self {
        Self {
            motion,
            delay,
            pending: FrameMap::default(),
            moves: FixedVec::new(),
            rest: f64::from(position),
        }
    }

    /// Frames from a write to the frame it is for.
    pub fn delay(&self) -> u32 {
        self.delay
    }

    /// Asks for `position` from frame `frame` on. Returns the frame start to write it at
    /// (`None`: write now, it is due in or before frame `current`).
    pub fn request(&mut self, frame: u64, position: i32, current: Option<u64>) -> Option<u64> {
        let write = frame.saturating_sub(u64::from(self.delay));
        match current {
            Some(c) if write <= c => None,
            _ => {
                self.pending.insert(write, position);
                Some(write)
            }
        }
    }

    /// Frame `seq` started: the position to write now, if one is due (the latest of them).
    pub fn due(&mut self, seq: u64) -> Option<i32> {
        self.pending.take_up_to(seq)
    }

    /// Records a move written at `at` (`CLOCK_MONOTONIC`).
    pub fn written(&mut self, at: Duration, position: i32) {
        let (from, _) = self.at(at);
        let m = Move {
            at,
            from,
            to: f64::from(position),
        };
        if self.moves.len() == MOVES_KEPT {
            self.rest = self.moves.remove(0).to;
        }
        let _ = self.moves.push(m);
    }

    /// The predicted position at `t` and whether the lens had settled.
    pub fn at(&self, t: Duration) -> (f64, bool) {
        let Some(m) = self.moves.iter().rev().find(|m| m.at <= t) else {
            return (self.rest, true);
        };
        self.motion.position(m.from, m.to, t - m.at)
    }

    /// Where the lens was for an exposure from `start` to `end` (`CLOCK_MONOTONIC`).
    pub fn frame(&self, start: Duration, end: Duration) -> LensFrame {
        let mid = start + (end.saturating_sub(start)) / 2;
        let (position, _) = self.at(mid);
        let (_, settled_start) = self.at(start);
        // Settled for the whole exposure: no move written during it, settled at its start.
        let moved = self.moves.iter().any(|m| m.at > start && m.at <= end);
        let target = self
            .moves
            .iter()
            .rev()
            .find(|m| m.at <= end)
            .map_or(self.rest, |m| m.to);
        LensFrame {
            position,
            settled: settled_start && !moved,
            target: target as i32,
        }
    }

    /// The position last written (or the rest position).
    pub fn target(&self) -> i32 {
        self.moves.last().map_or(self.rest, |m| m.to) as i32
    }
}

/// IMX708 phase detection data: 16×12 cells of (confidence, phase) from the third line of
/// its embedded data. Decoding from Raspberry Pi's `cam_helper_imx708.cpp`
/// (`parsePdafData`; BSD-2-Clause, Copyright (C) 2022 Raspberry Pi Ltd): in RAW10 or RAW12
/// packing (`bits_per_pixel`), each cell is `bits_per_pixel / 2` bytes after a two-cell
/// header; confidence is 11 bits, phase a signed 11-bit value (1/16 pixel) when confidence
/// is non-zero.
pub mod imx708_pdaf {
    use alloc::vec::Vec;

    /// Cells across.
    pub const COLUMNS: usize = 16;
    /// Cells down.
    pub const ROWS: usize = 12;

    /// One cell.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Cell {
        /// Confidence (0: none).
        pub conf: u16,
        /// Phase in 1/16 pixel (0 without confidence).
        pub phase: i16,
    }

    /// Decodes the PDAF line (the embedded data from its third line on). `None` when it is
    /// not PDAF data in a supported packing.
    pub fn parse(line: &[u8], bits_per_pixel: u32) -> Option<Vec<Cell>> {
        let step = (bits_per_pixel / 2) as usize;
        if !(10..=14).contains(&bits_per_pixel)
            || line.len() < (ROWS * COLUMNS + 2) * step
            || line[0] != 0
            || line[1] >= 0x40
        {
            return None;
        }
        let mut out = Vec::with_capacity(ROWS * COLUMNS);
        let mut p = &line[2 * step..];
        for _ in 0..ROWS * COLUMNS {
            let conf = (u16::from(p[0]) << 3) | u16::from(p[1] >> 5);
            let hi = i16::from(p[1] & 0x0f) - i16::from(p[1] & 0x10);
            let phase = (hi << 6) | i16::from(p[2] >> 2);
            out.push(Cell {
                conf,
                phase: if conf != 0 { phase } else { 0 },
            });
            p = &p[step..];
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_formats_are_lemnos() {
        let lens = |chip, format| VcmI2c {
            address: 0x0c,
            bus: None,
            chip,
            format,
        };
        let encode = |v: &VcmI2c, p| {
            v.with_format(|f| {
                let (m, n, _) = f.encode(p);
                m[..n].to_vec()
            })
        };
        // DW9714: 0x1ff << 4.
        assert_eq!(encode(&lens(VcmChip::Dw9714, None), 0x1ff), [0x1f, 0xf0]);
        assert_eq!(
            encode(&lens(VcmChip::Dw9807, None), 5000),
            [0x03, 0x03, 0xff]
        );
        assert_eq!(lens(VcmChip::Ak7375, None).max_position(), 4095);
        let custom = VcmFormat {
            register: Some(0x10),
            bytes: 2,
            shift: 0,
            bits: 10,
            or: 0,
            power_up: vec![vec![0x02, 0x00]],
            power_up_us: 100,
            power_down: vec![],
        };
        let v = lens(VcmChip::Custom, Some(custom.clone()));
        assert!(v.check().is_ok());
        assert_eq!(encode(&v, 0x2a5), [0x10, 0x02, 0xa5]);
        v.with_format(|f| assert_eq!(f.power_up, [&[0x02, 0x00][..]]));
        assert!(lens(VcmChip::Custom, None).check().is_err());
        let bad = VcmFormat { bytes: 1, ..custom };
        assert!(lens(VcmChip::Custom, Some(bad)).check().is_err());
    }

    #[test]
    fn description_parses() {
        let d: LensDescription = toml::from_str(
            "range = [0, 1023]\nsettle_us = 10000\nmap = [0.0, 445, 15.0, 925]\n\
             i2c = { address = 0x0c, chip = \"dw9817\" }",
        )
        .unwrap();
        d.check().unwrap();
        assert_eq!(d.delay, 2);
        assert_eq!(d.motion().settle, Duration::from_millis(10));
        assert!(toml::from_str::<LensDescription>("speed = 1").is_err());
    }

    #[test]
    fn imx708_data_has_its_lens_and_pdaf() {
        let all = crate::KernelSensorData::builtin();
        let d = crate::KernelSensorData::find(&all, "imx708 10-001a", true).unwrap();
        let lens = d.lens.as_ref().unwrap();
        lens.check().unwrap();
        assert!(lens.matches("dw9807 10-000c") && !lens.matches("ak7375 10-000c"));
        assert_eq!(lens.range(), [0, 1023]);
        assert_eq!(lens.map, [0.0, 445.0, 15.0, 925.0]);
        assert_eq!(d.pdaf.as_deref(), Some("imx708"));
    }

    #[test]
    fn schedule_writes_early_and_predicts_frames() {
        let ms = Duration::from_millis;
        let mut s = LensSchedule::new(LensMotion { settle: ms(10) }, 2, 400);
        // For frame 12, written at the start of frame 10.
        assert_eq!(s.request(12, 500, Some(8)), Some(10));
        assert_eq!(s.due(9), None);
        assert_eq!(s.due(10), Some(500));
        s.written(ms(330), 500);
        // A frame exposed 300-320 ms saw the rest position; 330-345 a moving lens; 345-360
        // the new position, settled.
        let f = s.frame(ms(300), ms(320));
        assert_eq!((f.position, f.settled), (400.0, true));
        let f = s.frame(ms(330), ms(345));
        assert!(!f.settled && f.position > 400.0 && f.position < 500.0);
        let f = s.frame(ms(345), ms(360));
        assert_eq!((f.position, f.settled, f.target), (500.0, true, 500));
        // Late: written at once.
        assert_eq!(s.request(11, 520, Some(10)), None);
    }

    #[test]
    fn imx708_pdaf_cells() {
        // RAW10: 5 bytes per cell; cell 0 conf 0x123, phase -5; cell 1 no confidence.
        let mut line = vec![0u8; (16 * 12 + 2) * 5];
        let at = 2 * 5;
        let phase: i16 = -5;
        line[at] = (0x123 >> 3) as u8;
        line[at + 1] = ((0x123 & 7) << 5) as u8 | (((phase >> 6) as u8) & 0x1f);
        line[at + 2] = ((phase & 0x3f) as u8) << 2;
        line[at + 5 + 2] = 0xff;
        let cells = imx708_pdaf::parse(&line, 10).unwrap();
        assert_eq!(cells.len(), 192);
        assert_eq!(
            cells[0],
            imx708_pdaf::Cell {
                conf: 0x123,
                phase: -5
            }
        );
        assert_eq!(cells[1], imx708_pdaf::Cell::default());
        line[1] = 0x40;
        assert!(imx708_pdaf::parse(&line, 10).is_none());
        assert!(imx708_pdaf::parse(&line[..100], 10).is_none());
    }
}
