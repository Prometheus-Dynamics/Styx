//! Tuning: typed, per-algorithm data.
//!
//! Our own format is TOML mirroring [`Tuning`] (unknown keys rejected, missing keys defaulted).
//! Raspberry Pi tuning files (JSON, version 2, as used by libcamera's rpi IPA) are converted with
//! [`Tuning::from_rpi_json_str`]; see `docs/native-stack/algorithms.md` for the mapping.
//!
//! Units: times in microseconds (`*_us`), levels normalised to full scale 1.0, colour
//! temperatures in kelvin.

pub mod json;
mod rpi;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use crate::algos::af::tuning::{AfRangeTuning, AfRanges, AfSpeedTuning, AfSpeeds, AfTuning};
pub use crate::algos::agc::tuning::{AgcTuning, Bound, Constraint, ExposureProfile, MeteringMode};
pub use crate::algos::alsc::{AlscCalibration, AlscTuning};
pub use crate::algos::awb::tuning::{AwbMode, AwbPrior, AwbTuning};
pub use crate::algos::black_level::{BlackLevelTuning, GainBlackLevel};
pub use crate::algos::ccm::{CcmTuning, CtCcm};
pub use crate::algos::contrast::ContrastTuning;
pub use crate::algos::denoise::{
    CdnTuning, DenoiseTuning, GeqTuning, NoiseTuning, SdnTuning, SharpenTuning, TdnTuning,
};
pub use crate::algos::lux::LuxTuning;
use crate::error::{AlgoError, Result};
pub use rpi::RpiImport;

/// A camera's tuning. Absent sections use each algorithm's defaults (see
/// [`crate::Pipeline::from_tuning`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Tuning {
    /// Free-form description (sensor, lens, source of the data).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Black level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub black_level: Option<BlackLevelTuning>,
    /// Lux estimation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lux: Option<LuxTuning>,
    /// Automatic exposure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agc: Option<AgcTuning>,
    /// Automatic white balance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awb: Option<AwbTuning>,
    /// Lens shading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alsc: Option<AlscTuning>,
    /// Colour correction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ccm: Option<CcmTuning>,
    /// Tone curve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<ContrastTuning>,
    /// Noise profile, denoise, green equalisation, defective pixels and sharpening (ISP
    /// blocks only some ISPs have).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denoise: Option<DenoiseTuning>,
    /// Autofocus (cameras with a focus lens; the generic defaults without a section).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub af: Option<AfTuning>,
}

impl Tuning {
    /// Parse our TOML format.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let t: Tuning = toml::from_str(text).map_err(|e| AlgoError::Toml(e.to_string()))?;
        t.validate()?;
        Ok(t)
    }

    /// Write our TOML format.
    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string(self).map_err(|e| AlgoError::Toml(e.to_string()))
    }

    /// Convert a Raspberry Pi tuning file (JSON, version 2). The report lists what was not
    /// used.
    pub fn from_rpi_json_str(text: &str) -> Result<RpiImport> {
        let import = rpi::convert(&json::parse(text)?)?;
        import.tuning.validate()?;
        Ok(import)
    }

    /// Write a Raspberry Pi tuning file (version 2) for `target` (`pisp` or `bcm2835`; `None`:
    /// `bcm2835` for 16×12 lens shading tables, else `pisp`). Styx-only settings are left out;
    /// see `docs/native-stack/algorithms.md`.
    pub fn to_rpi_json_string(&self, target: Option<&str>) -> String {
        rpi::export(self, target)
    }

    /// Load a file: `.json` as a Raspberry Pi tuning, anything else as our TOML.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)?;
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            Ok(Self::from_rpi_json_str(&text)?.tuning)
        } else {
            Self::from_toml_str(&text)
        }
    }

    /// Check every section.
    pub fn validate(&self) -> Result<()> {
        if let Some(b) = &self.black_level {
            b.validate()?;
        }
        if let Some(l) = &self.lux {
            l.validate()?;
        }
        if let Some(a) = &self.agc {
            a.validate()?;
        }
        if let Some(a) = &self.awb {
            a.validate()?;
        }
        if let Some(a) = &self.alsc {
            a.validate()?;
        }
        if let Some(c) = &self.ccm {
            c.validate()?;
        }
        if let Some(c) = &self.contrast {
            c.validate()?;
        }
        if let Some(d) = &self.denoise {
            d.validate()?;
        }
        if let Some(a) = &self.af {
            a.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_the_default() {
        assert_eq!(Tuning::from_toml_str("").unwrap(), Tuning::default());
    }

    #[test]
    fn toml_sections_parse_with_defaults() {
        let t = Tuning::from_toml_str(
            r#"
            description = "example"
            [black_level]
            r = 0.0625
            g = 0.0625
            b = 0.0625

            [agc]
            y_target = [0, 0.16, 1000, 0.17]
            default_metering_mode = "spot"

            [awb]
            ct_curve = [[2800, 0.9, 0.4], [6000, 0.5, 0.8]]
            priors = [{ lux = 0, prior = [2000, 1, 8000, 0] }]
            modes = { auto = { lo = 2500, hi = 8000 } }
            "#,
        )
        .unwrap();
        let agc = t.agc.unwrap();
        assert_eq!(agc.default_metering_mode, "spot");
        assert_eq!(agc.speed, 0.2);
        assert!(agc.exposure_modes.contains_key("normal"));
        assert!(t.awb.unwrap().uses_bayes());
    }

    #[test]
    fn toml_rejects_unknown_keys_and_bad_values() {
        assert!(Tuning::from_toml_str("[agc]\nspeeed = 1").is_err());
        assert!(Tuning::from_toml_str("[agc]\nspeed = 2.0").is_err());
    }
}
