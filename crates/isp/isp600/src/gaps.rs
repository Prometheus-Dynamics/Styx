//! What the ISP600 mapping cannot encode yet, named per block.
//!
//! The Lemnos library holds the load image's regions, the register offsets and the module and
//! update bits the kernel names. It does not hold how a value is written into them: the register
//! encodings of the black level, gains, colour matrix, tone curve, shading, denoise, sharpening
//! and histogram weights, the table entries behind them, or how the statistics bytes decode into
//! `styx_aaa::Statistics`. Those are the gaps. This crate does not guess them: [`unmapped`] names
//! each non-neutral setting that would need one of them, and [`GAPS`] says what is missing.

use styx_aaa::{DenoiseParams, IDENTITY};
use styx_settings::IspSettings;

/// One block or path whose values the library cannot yet write or read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// The block, by its library name.
    pub block: &'static str,
    /// What the library would need to provide for Styx to map it.
    pub needs: &'static str,
}

/// The gaps, one per block, in the order of the load image.
pub const GAPS: &[Gap] = &[
    Gap {
        block: "BLC (black level)",
        needs: "the register encoding of the per-channel black levels",
    },
    Gap {
        block: "WB and DG (white balance, digital gain)",
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
        block: "statistics (AE, AWB, AF, AFS, histograms, tone, denoise)",
        needs: "the field encodings inside each statistics region, to decode into styx_aaa::Statistics",
    },
];

/// The non-neutral fields of `settings`, by name: each needs an encoding named in [`GAPS`]. Empty
/// for neutral settings, which need none. Allocates nothing.
pub fn unmapped(settings: &IspSettings) -> impl Iterator<Item = &'static str> {
    let fields = [
        ("black_level", settings.black_level != 0.0),
        ("black_channels", settings.black_channels.is_some()),
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
