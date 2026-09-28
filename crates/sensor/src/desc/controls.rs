//! The `[controls]` section: exposure, gain, frame length, delays, group hold, flips, test
//! patterns and the embedded data layout.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::step::Step;
use super::types::Field;

/// Sensor controls and how they map to registers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Controls {
    /// Frame length (VTS) register, in lines. Required for register-driven sensors.
    #[serde(default)]
    pub frame_length: Option<Field>,
    /// Line length (HTS) register.
    #[serde(default)]
    pub line_length: Option<LineLength>,
    /// Exposure (coarse integration time).
    pub exposure: Exposure,
    /// Analogue gain.
    pub analog_gain: Gain,
    /// Digital gain, if the sensor has one.
    #[serde(default)]
    pub digital_gain: Option<Gain>,
    /// Frames between writing a control and the first frame it affects.
    #[serde(default)]
    pub delays: Delays,
    /// Group hold (grouped register update) registers, if the sensor has them.
    #[serde(default)]
    pub group_hold: Option<GroupHold>,
    /// Horizontal mirror bit.
    #[serde(default)]
    pub hflip: Option<Flip>,
    /// Vertical flip bit.
    #[serde(default)]
    pub vflip: Option<Flip>,
    /// Test pattern register.
    #[serde(default)]
    pub test_pattern: Option<TestPattern>,
}

/// Line length (HTS) register. `line_length_pixels = register * pixels_per_unit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineLength {
    /// The register.
    pub register: Field,
    /// Pixels per register unit (some sensors count HTS in pairs of pixels).
    #[serde(default = "one")]
    pub pixels_per_unit: u32,
}

fn one() -> u32 {
    1
}

/// Exposure in lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exposure {
    /// Register field holding the integer line count. The `fraction_bits` fractional bits sit
    /// directly below it. Absent for kernel-driven sensors.
    #[serde(default)]
    pub register: Option<Field>,
    /// Fractional line bits below the integer field (0 when unsupported).
    #[serde(default)]
    pub fraction_bits: u8,
    /// Minimum exposure in lines.
    pub min: u32,
    /// Exposure may be at most `frame_length - margin` lines.
    pub margin: u32,
    /// Step in lines.
    #[serde(default = "one")]
    pub step: u32,
    /// Exposure written when a mode is applied, in lines.
    pub default: u32,
}

/// A gain control.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gain {
    /// The register field. Absent for kernel-driven sensors.
    #[serde(default)]
    pub register: Option<Field>,
    /// Lowest valid code.
    pub min_code: u32,
    /// Highest valid code.
    pub max_code: u32,
    /// Code written when a mode is applied.
    pub default_code: u32,
    /// How codes map to gain.
    pub model: GainModel,
}

/// Mapping between register codes and gain ratios.
///
/// ```toml
/// model = { linear = { step = 0.0625 } }                      # gain = code * step
/// model = { linear = { step = 0.0625, offset = 0 } }          # gain = (code + offset) * step
/// model = { reciprocal = { numerator = 256, base = 256 } }    # gain = numerator / (base - code)
/// model = { table = [[0x00, 1.0], [0x10, 2.0], [0x30, 4.0]] } # code -> gain pairs
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GainModel {
    /// `gain = (code + offset) * step`.
    Linear {
        /// Gain per code.
        step: f64,
        /// Added to the code first.
        #[serde(default)]
        offset: i64,
    },
    /// `gain = numerator / (base - code)` (common on Sony sensors).
    Reciprocal {
        /// Numerator.
        numerator: f64,
        /// Base.
        base: f64,
    },
    /// Explicit code to gain pairs, gain increasing.
    Table(Vec<(u32, f64)>),
}

/// Control delays in frames: a value written during frame N first affects frame N + delay.
/// With group hold, the writes of one frame are held and launched together and the delays
/// count from the frame in which the group was launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Delays {
    /// Exposure.
    pub exposure: u32,
    /// Analogue gain.
    pub analog_gain: u32,
    /// Digital gain.
    pub digital_gain: u32,
    /// Frame length (vertical blanking).
    pub frame_length: u32,
}

impl Default for Delays {
    /// libcamera's defaults for sensors without a helper: exposure 2, gain 1, vblank 2.
    fn default() -> Self {
        Self {
            exposure: 2,
            analog_gain: 1,
            digital_gain: 1,
            frame_length: 2,
        }
    }
}

/// Register sequences that bracket a grouped update.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupHold {
    /// Written before the grouped writes.
    pub start: Vec<Step>,
    /// Written after the grouped writes.
    #[serde(default)]
    pub end: Vec<Step>,
    /// Written after `end` to launch the group (empty when `end` launches it).
    #[serde(default)]
    pub launch: Vec<Step>,
}

/// A flip or mirror bit, always written read-modify-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flip {
    /// Register address (one byte).
    pub address: u16,
    /// Bit mask to set for "flipped".
    pub mask: u8,
    /// Applied with the mode.
    #[serde(default)]
    pub default: bool,
    /// Whether flipping changes the Bayer order of the output.
    #[serde(default)]
    pub changes_bayer_order: bool,
}

/// Test pattern register and its named values.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestPattern {
    /// The register field.
    pub register: Field,
    /// Pattern name to field value; must include `off`.
    pub patterns: BTreeMap<String, u32>,
}

/// Where applied register values appear in the sensor's embedded data lines.
///
/// `entries` give the byte offset (into the unpacked embedded data) of each register byte;
/// multi-byte registers list each address. Only the controls whose every register byte is
/// listed can be read back.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedData {
    /// Embedded data lines at the top of each frame.
    pub lines: u32,
    /// Register byte locations.
    pub entries: Vec<EmbeddedEntry>,
}

/// One register byte in embedded data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedEntry {
    /// Register address.
    pub address: u16,
    /// Byte offset in the unpacked embedded data.
    pub offset: u32,
}
