//! Tuning: typed, per-algorithm parameters.
//!
//! The structs mirror a camera's tuning section by section (absent sections use each
//! algorithm's defaults). Reading tuning files is not here.
//!
//! Units: times in microseconds (`*_us`), levels normalised to full scale 1.0, colour
//! temperatures in kelvin.

use alloc::string::String;

pub use crate::algos::af::tuning::{AfRangeTuning, AfRanges, AfSpeedTuning, AfSpeeds, AfTuning};
pub use crate::algos::agc::tuning::{AgcTuning, Bound, Constraint, ExposureProfile, MeteringMode};
pub use crate::algos::alsc::{AlscCalibration, AlscTuning};
pub use crate::algos::awb::tuning::{AwbMode, AwbPrior, AwbTuning};
pub use crate::algos::black_level::{BlackLevelTuning, GainBlackLevel};
pub use crate::algos::ccm::{CcmTuning, CtCcm};
pub use crate::algos::contrast::ContrastTuning;
pub use crate::algos::denoise::{
    CdnTuning, DenoiseTuning, GeqTuning, NoiseGain, NoiseTuning, SdnTuning, SharpenTuning,
    TdnTuning,
};
pub use crate::algos::lux::LuxTuning;
use crate::error::Result;

#[cfg(feature = "serde")]
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A camera's tuning. Absent sections use each algorithm's defaults (see
/// [`crate::Pipeline::from_tuning`]).
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields, default))]
pub struct Tuning {
    /// Free-form description (sensor, lens, source of the data).
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub description: Option<String>,
    /// Black level.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub black_level: Option<BlackLevelTuning>,
    /// Lux estimation.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub lux: Option<LuxTuning>,
    /// Automatic exposure.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub agc: Option<AgcTuning>,
    /// Automatic white balance.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub awb: Option<AwbTuning>,
    /// Lens shading.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub alsc: Option<AlscTuning>,
    /// Colour correction.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub ccm: Option<CcmTuning>,
    /// Tone curve.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub contrast: Option<ContrastTuning>,
    /// Noise profile, denoise, green equalisation, defective pixels and sharpening (ISP
    /// blocks only some ISPs have).
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub denoise: Option<DenoiseTuning>,
    /// Autofocus (cameras with a focus lens; the generic defaults without a section).
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub af: Option<AfTuning>,
}

impl Tuning {
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
