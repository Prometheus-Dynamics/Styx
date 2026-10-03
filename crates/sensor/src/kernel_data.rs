//! What a kernel driver does not report about its sensor, as data: the gain code model, the
//! control delays, the black level, the embedded data layout, the tuning file. libcamera keeps
//! the same facts in its per-sensor `CamHelper` classes; here they are a small TOML file, and a
//! sensor without one still works with generic defaults (see
//! [`SensorDescription::from_subdev_with`](crate::SensorDescription::from_subdev_with)).
//!
//! ```toml
//! name = "imx219"                 # the sensor; also the default driver name to match
//! drivers = ["imx219"]            # entity names (first word) this file applies to
//! vendor = "Sony"
//! verified = false                # checked on hardware with Styx?
//! tuning = "imx219.json"
//! colour = true                   # only for sensors whose driver reports Bayer codes
//! analog_gain = { reciprocal = { numerator = 256, base = 256 } }
//! digital_gain = { linear = { step = 0.00390625 } }
//! delays = { exposure = 2, analog_gain = 1, frame_length = 2 }
//! black_level = { value = 64, bits = 10 }
//! exposure_margin = 4             # default: what the driver's EXPOSURE maximum implies
//! frame_length_extra_lines = 0
//! pdaf = "imx708"                 # phase detection data in the embedded data
//! [lens]                          # a focus lens its kernel driver moves (styx_sensor::lens)
//! settle_us = 12000
//! map = [0.0, 445, 15.0, 925]
//! [embedded_data]                 # as in a sensor description
//! lines = 2
//! format = "ccs"
//! registers = [{ control = "exposure", address = 0x015a, bytes = 2 }]
//! ```

use serde::Deserialize;

use crate::desc::{BlackLevel, Delays, EmbeddedData, GainModel};
use crate::error::{Result, SensorError};

/// Facts about a sensor a kernel driver drives that the driver does not report.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelSensorData {
    /// Sensor name, e.g. `imx219`.
    pub name: String,
    /// Driver entity names (the first word of the media entity name) this applies to; empty
    /// means just `name`.
    #[serde(default)]
    pub drivers: Vec<String>,
    /// Vendor, informational.
    #[serde(default)]
    pub vendor: Option<String>,
    /// Checked on hardware with Styx (else taken from public sources only).
    #[serde(default)]
    pub verified: bool,
    /// Where the values come from, informational.
    #[serde(default)]
    pub source: Option<String>,
    /// Tuning file name.
    #[serde(default)]
    pub tuning: Option<String>,
    /// Applies only when the driver reports colour (`true`: Bayer) or mono (`false`) codes; a
    /// driver for a family (`ov9282` drives the mono OV9281/OV9282 and the colour OV9782) is
    /// told apart by this.
    #[serde(default)]
    pub colour: Option<bool>,
    /// Analogue gain codes (`ANALOGUE_GAIN`) to gain. Default: linear with the driver's
    /// minimum code as 1×.
    #[serde(default)]
    pub analog_gain: Option<GainModel>,
    /// Digital gain codes (`DIGITAL_GAIN`) to gain. Default: linear with the driver's default
    /// code as 1×.
    #[serde(default)]
    pub digital_gain: Option<GainModel>,
    /// Frames between setting a control and the first frame it affects. Default: libcamera's
    /// for unknown sensors (exposure 2, gain 1, frame length 2).
    #[serde(default)]
    pub delays: Option<Delays>,
    /// Black level.
    #[serde(default)]
    pub black_level: Option<BlackLevel>,
    /// Exposure is at most `frame length - margin` lines. Default: what the driver's
    /// `EXPOSURE` maximum implies at the current frame length.
    #[serde(default)]
    pub exposure_margin: Option<u32>,
    /// Lines a frame lasts beyond `height + VBLANK`.
    #[serde(default)]
    pub frame_length_extra_lines: u32,
    /// Embedded data layout, when the driver sends it on a metadata pad.
    #[serde(default)]
    pub embedded_data: Option<EmbeddedData>,
    /// The focus lens a kernel lens driver moves (settle time, the dioptre map), for modules
    /// with one; a lens linked to the sensor without this gets the defaults.
    #[serde(default)]
    pub lens: Option<crate::lens::LensDescription>,
    /// Phase detection data in the embedded data (`"imx708"`), for AF.
    #[serde(default)]
    pub pdaf: Option<String>,
}

/// The data files that ship with this crate (`sensors/kernel/*.toml`).
pub const BUILTIN_KERNEL_DATA: &[(&str, &str)] = &[
    ("ov9782", include_str!("../sensors/kernel/ov9782.toml")),
    ("ov9281", include_str!("../sensors/kernel/ov9281.toml")),
    ("imx219", include_str!("../sensors/kernel/imx219.toml")),
    ("imx477", include_str!("../sensors/kernel/imx477.toml")),
    ("imx708", include_str!("../sensors/kernel/imx708.toml")),
    ("ov5647", include_str!("../sensors/kernel/ov5647.toml")),
];

impl KernelSensorData {
    /// Parses a data file. `source_name` labels errors.
    pub fn from_toml_str(source: &str, source_name: &str) -> Result<Self> {
        toml::from_str(source).map_err(|e| SensorError::Parse {
            source_name: source_name.to_owned(),
            message: e.to_string(),
        })
    }

    /// Reads and parses a data file.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let src = std::fs::read_to_string(path).map_err(|source| SensorError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&src, &path.display().to_string())
    }

    /// The data files that ship with this crate.
    pub fn builtin() -> Vec<Self> {
        BUILTIN_KERNEL_DATA
            .iter()
            .filter_map(|(n, src)| Self::from_toml_str(src, &format!("builtin:{n}")).ok())
            .collect()
    }

    /// Whether this applies to a driver entity named `entity` (e.g. `imx219 10-0010`; the
    /// first word is compared) reporting colour (`true`) or mono codes.
    pub fn matches(&self, entity: &str, colour: bool) -> bool {
        let driver = entity.split_whitespace().next().unwrap_or("");
        let named = if self.drivers.is_empty() {
            self.name == driver
        } else {
            self.drivers.iter().any(|d| d == driver)
        };
        named && self.colour.is_none_or(|c| c == colour)
    }

    /// The first of `candidates` that applies (see [`Self::matches`]).
    pub fn find<'a>(candidates: &'a [Self], entity: &str, colour: bool) -> Option<&'a Self> {
        candidates.iter().find(|d| d.matches(entity, colour))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_files_parse_and_match_their_drivers() {
        let all = KernelSensorData::builtin();
        assert_eq!(all.len(), BUILTIN_KERNEL_DATA.len(), "every file parses");
        let find = |e, c| KernelSensorData::find(&all, e, c).map(|d| d.name.as_str());
        assert_eq!(find("imx219 10-0010", true), Some("imx219"));
        assert_eq!(find("imx477 10-001a", true), Some("imx477"));
        assert_eq!(find("imx708 10-001a", true), Some("imx708"));
        assert_eq!(find("ov5647 10-0036", true), Some("ov5647"));
        // The ov9282 driver drives both: told apart by the codes it reports.
        assert_eq!(find("ov9282 10-0060", true), Some("ov9782"));
        assert_eq!(find("ov9282 10-0060", false), Some("ov9281"));
        assert_eq!(find("ov9782 10-0060", true), Some("ov9782"));
        assert_eq!(find("imx999 10-0010", true), None);
        for d in &all {
            assert!(d.delays.is_some() && d.analog_gain.is_some(), "{}", d.name);
            if let Some(e) = &d.embedded_data {
                assert!(!e.registers.is_empty(), "{}", d.name);
            }
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let e = KernelSensorData::from_toml_str("name = \"x\"\ngain = 1\n", "t").unwrap_err();
        assert!(e.to_string().contains("unknown field"), "{e}");
    }
}
