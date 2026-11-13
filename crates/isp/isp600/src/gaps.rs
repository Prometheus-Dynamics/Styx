//! The settings this block cannot be given, named one by one.
//!
//! The Lemnos library's `tuning` module writes the black level's enables and bit depths, the
//! module enables and the temporal denoise's record enable. It writes nothing else: the register
//! encodings of white balance, the colour matrix, the gamma and tone curves, lens shading, the
//! spatial denoise strengths, sharpening and the statistics decodes have no open source, so this
//! crate does not invent them. A setting outside the supported set is refused with an
//! [`Unavailable`] error, never written as a guess, and never dropped silently.
//!
//! The default image path for this board family is raw from the receiver into the software ISP
//! (`styx-soft`) or the GPU ISP (`styx-gpu`), with Styx's own 3A. The hardware ISP is optional
//! and only useful for the black level and the enables that [`crate::load`] writes.

use styx_aaa::{DenoiseParams, IDENTITY};
use styx_settings::IspSettings;

/// The message every refusal of a setting carries, before the setting's name.
pub const MESSAGE: &str = "hardware ISP tuning unavailable: no open source for the encodings";

/// A setting the hardware ISP cannot be given: its encoding has no open source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable {
    /// The field of [`IspSettings`] (or the block it feeds), by name.
    pub setting: &'static str,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{MESSAGE} ({})", self.setting)
    }
}

impl std::error::Error for Unavailable {}

/// One block whose encodings have no open source, and what they would need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// The block, by its library name.
    pub block: &'static str,
    /// What an open source would have to give for Styx to write it.
    pub needs: &'static str,
}

/// The unavailable blocks, in the order of the load image.
pub const GAPS: &[Gap] = &[
    Gap {
        block: "WB and DG (white balance, digital gain, deflicker gain)",
        needs: "the register encodings of the channel gains",
    },
    Gap {
        block: "CCM (colour matrix)",
        needs: "the register encoding of the nine matrix coefficients",
    },
    Gap {
        block: "GAMMA and DRC (tone curve)",
        needs: "the entry layout of the RGB table's gamma and dynamic range tables",
    },
    Gap {
        block: "RSC and MSC (lens shading)",
        needs: "the entry layout of the Bayer table's shading tables, and the per-band rows",
    },
    Gap {
        block: "D2D, D3D, CNR, DPC (denoise)",
        needs: "the register encodings of the denoise strengths and the temporal denoise tables",
    },
    Gap {
        block: "SHARP (sharpening)",
        needs: "the register encoding of the sharpening factors",
    },
    Gap {
        block: "HIST0 and HIST1 (histogram weights)",
        needs: "the register encoding of the zone weights",
    },
    Gap {
        block: "AE, AF, AWB, AFS, histograms, tone and denoise statistics",
        needs: "the statistics window encodings and the field decodes into styx_aaa::Statistics",
    },
];

/// The settings of `settings` that are not neutral and have no encoding here, in the order of
/// [`GAPS`]. Empty for neutral settings. Allocates nothing.
pub fn unavailable(settings: &IspSettings) -> impl Iterator<Item = &'static str> {
    let fields = [
        ("wb", settings.wb != [1.0; 3]),
        ("digital_gain", settings.digital_gain != 1.0),
        ("flicker", settings.flicker != 1.0),
        ("flicker_bands", settings.flicker_bands.is_some()),
        ("ccm", settings.ccm != IDENTITY),
        ("gamma", settings.gamma.is_some()),
        ("lens_shading", settings.lens_shading.is_some()),
        ("denoise", settings.denoise != DenoiseParams::default()),
        ("sharpen", settings.sharpen.is_some()),
        ("histogram_weights", settings.histogram_weights.is_some()),
    ];
    fields
        .into_iter()
        .filter_map(|(name, non_neutral)| non_neutral.then_some(name))
}

/// `Ok` when every setting is neutral or is one this block writes (the black level). Otherwise
/// the first unavailable setting, as an error naming it.
pub fn check(settings: &IspSettings) -> Result<(), Unavailable> {
    match unavailable(settings).next() {
        Some(setting) => Err(Unavailable { setting }),
        None => Ok(()),
    }
}
