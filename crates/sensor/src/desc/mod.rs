//! The sensor description schema. See the crate documentation for a full example.

mod controls;
mod step;
mod types;
mod validate;

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

pub use controls::{
    Controls, Delays, EmbeddedControl, EmbeddedControlKind, EmbeddedData, EmbeddedEntry,
    EmbeddedFormat, EmbeddedPacking, EmbeddedRegister, Exposure, Flip, Gain, GainModel, GroupHold,
    LineLength, TestPattern,
};
pub use step::{RegWrite, Step};
pub use types::{Blanking, Field, Rect, Size};

use crate::error::{Result, SensorError};
use crate::mbus::{ColorFilter, MbusCode};
use crate::timing::Timing;

/// A complete sensor description.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorDescription {
    /// Identity and bus details.
    pub sensor: Identity,
    /// Pixel array geometry and colour.
    pub pixel_array: PixelArray,
    /// Register sequences and power sequencing.
    #[serde(default)]
    pub sequences: Sequences,
    /// Output formats (bit depths) by name.
    pub formats: BTreeMap<String, Format>,
    /// Sensor modes.
    pub modes: Vec<Mode>,
    /// Controls and their registers.
    pub controls: Controls,
    /// Embedded data layout, if the sensor sends register values with each frame.
    #[serde(default)]
    pub embedded_data: Option<EmbeddedData>,
}

/// Who drives the sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Styx drives the sensor's registers from userspace (the native path).
    #[default]
    Registers,
    /// A kernel driver owns the sensor; controls go through V4L2 controls
    /// (`EXPOSURE`, `ANALOGUE_GAIN`, `VBLANK`, `HBLANK`). Register fields are absent.
    Kernel,
}

/// Identity and bus details.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// Sensor name, e.g. `ov9782`.
    pub name: String,
    /// Vendor, informational.
    #[serde(default)]
    pub vendor: Option<String>,
    /// Who drives the sensor.
    #[serde(default)]
    pub backend: Backend,
    /// 7-bit I²C address. Required for [`Backend::Registers`].
    #[serde(default)]
    pub i2c_address: Option<u16>,
    /// Register address width in bits: 8 or 16.
    #[serde(default = "sixteen")]
    pub address_bits: u8,
    /// Consecutive register writes may go out as one auto-incrementing burst (one bus
    /// transfer: address, then the bytes). Off by default: one transfer per write.
    #[serde(default)]
    pub burst_writes: bool,
    /// Chip identification register.
    #[serde(default)]
    pub chip_id: Option<ChipId>,
    /// Input clocks by role, in Hz (`{ xvclk = 24_000_000 }`).
    #[serde(default)]
    pub clocks: BTreeMap<String, u32>,
    /// Tuning file name (looked up by the algorithms layer).
    #[serde(default)]
    pub tuning: Option<String>,
}

fn sixteen() -> u8 {
    16
}

/// Chip identification register and the values that identify the sensor.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChipId {
    /// First register address.
    pub address: u16,
    /// Width in bytes (most significant first).
    #[serde(default = "one_u8")]
    pub bytes: u8,
    /// Accepted values.
    pub values: Vec<u32>,
}

fn one_u8() -> u8 {
    1
}

/// Pixel array geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PixelArray {
    /// Full array including dummy and optical-black pixels.
    pub size: Size,
    /// Active (image) area.
    pub active: Rect,
    /// Colour filter order, or `mono`.
    pub color_filter: ColorFilter,
    /// Black level.
    #[serde(default)]
    pub black_level: Option<BlackLevel>,
}

/// Black level, at a given bit depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlackLevel {
    /// The pedestal.
    pub value: u32,
    /// Bit depth `value` is expressed at.
    pub bits: u8,
    /// Frames at the start of a stream that read a higher black level while the sensor's
    /// black level calibration settles (their levels mislead exposure control).
    #[serde(default)]
    pub settle_frames: u32,
}

impl BlackLevel {
    /// The black level at another bit depth.
    pub fn at_bits(&self, bits: u8) -> f64 {
        f64::from(self.value) * 2f64.powi(i32::from(bits) - i32::from(self.bits))
    }
}

/// Register sequences.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sequences {
    /// Power-up: supplies, clocks, GPIO lines, delays and any writes needed right after.
    #[serde(default)]
    pub power_up: Vec<Step>,
    /// Power-down.
    #[serde(default)]
    pub power_down: Vec<Step>,
    /// Common registers written after power-up, before any mode.
    #[serde(default)]
    pub init: Vec<Step>,
    /// Start streaming.
    #[serde(default)]
    pub stream_on: Vec<Step>,
    /// Stop streaming.
    #[serde(default)]
    pub stream_off: Vec<Step>,
}

/// An output format (bit depth).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Format {
    /// Media bus code.
    pub code: MbusCode,
    /// Bits per pixel (default: from the code).
    #[serde(default)]
    pub bit_depth: Option<u8>,
    /// Pixel rate in pixels per second (V4L2 `PIXEL_RATE`), unless the mode overrides it.
    pub pixel_rate: u64,
    /// CSI-2 link frequency in Hz (V4L2 `LINK_FREQ`).
    #[serde(default)]
    pub link_frequency: Option<u64>,
    /// Registers written for this format, before the mode's registers.
    #[serde(default)]
    pub registers: Vec<Step>,
}

impl Format {
    /// Bits per pixel.
    pub fn bits(&self) -> u8 {
        self.bit_depth
            .or_else(|| self.code.bit_depth())
            .unwrap_or(0)
    }
}

/// A sensor mode.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    /// Name, unique within the description.
    pub name: String,
    /// Output size.
    pub size: Size,
    /// Area of the pixel array read out (before binning / skipping).
    pub crop: Rect,
    /// Formats this mode supports (default: all).
    #[serde(default)]
    pub formats: Option<Vec<String>>,
    /// Horizontal and vertical binning factors.
    #[serde(default = "unit")]
    pub binning: [u32; 2],
    /// Horizontal and vertical skipping (subsampling) factors.
    #[serde(default = "unit")]
    pub skipping: [u32; 2],
    /// Horizontal blanking in pixels; `line_length = width + hblank`.
    pub hblank: Blanking,
    /// Vertical blanking in lines; `frame_length = height + vblank`.
    pub vblank: Blanking,
    /// Pixel rate override for this mode (all formats).
    #[serde(default)]
    pub pixel_rate: Option<u64>,
    /// Mode registers, written after the format's.
    #[serde(default)]
    pub registers: Vec<Step>,
}

fn unit() -> [u32; 2] {
    [1, 1]
}

impl Mode {
    /// Whether the mode supports the named format.
    pub fn supports(&self, format: &str) -> bool {
        self.formats
            .as_ref()
            .is_none_or(|f| f.iter().any(|n| n == format))
    }
}

impl SensorDescription {
    /// Parse and validate a description. `source_name` labels error messages.
    pub fn from_toml_str(source: &str, source_name: &str) -> Result<Self> {
        let desc: Self = toml::from_str(source).map_err(|e| SensorError::Parse {
            source_name: source_name.to_owned(),
            message: e.to_string(),
        })?;
        desc.validate().map_err(|issues| SensorError::Invalid {
            source_name: source_name.to_owned(),
            issues,
        })?;
        Ok(desc)
    }

    /// Read, parse and validate a description file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let src = std::fs::read_to_string(path).map_err(|source| SensorError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&src, &path.display().to_string())
    }

    /// Check the description for consistency. Returns every problem found.
    pub fn validate(&self) -> std::result::Result<(), crate::Issues> {
        validate::validate(self)
    }

    /// The mode with this name.
    pub fn mode(&self, name: &str) -> Result<&Mode> {
        self.modes
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| SensorError::UnknownMode(name.into()))
    }

    /// The format with this name, checked against the mode.
    pub fn format_for(&self, mode: &Mode, format: &str) -> Result<&Format> {
        match self.formats.get(format) {
            Some(f) if mode.supports(format) => Ok(f),
            _ => Err(SensorError::UnknownFormat {
                mode: mode.name.clone(),
                format: format.into(),
            }),
        }
    }

    /// The formats a mode supports, by name.
    pub fn formats_of<'a>(
        &'a self,
        mode: &'a Mode,
    ) -> impl Iterator<Item = (&'a str, &'a Format)> + 'a {
        self.formats
            .iter()
            .filter(|(n, _)| mode.supports(n))
            .map(|(n, f)| (n.as_str(), f))
    }

    /// The timing model for a mode and format, at the mode's default horizontal blanking.
    pub fn timing(&self, mode: &str, format: &str) -> Result<Timing> {
        let m = self.mode(mode)?;
        let f = self.format_for(m, format)?;
        Ok(Timing::for_mode(m, f, &self.controls.exposure)
            .with_extra_lines(self.controls.frame_length_extra_lines))
    }

    /// Output colour filter order for the given flips.
    pub fn color_filter(&self, hflip: bool, vflip: bool) -> ColorFilter {
        let changes = |f: &Option<Flip>| f.is_some_and(|f| f.changes_bayer_order);
        self.pixel_array.color_filter.flipped(
            hflip && changes(&self.controls.hflip),
            vflip && changes(&self.controls.vflip),
        )
    }
}
