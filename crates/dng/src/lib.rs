//! DNG (Adobe Digital Negative, version 1.4) raw files in pure Rust.
//!
//! * [`write_dng`]: a raw Bayer or monochrome frame ([`RawImage`], from 8/10/12/14/16-bit
//!   samples or CSI-2 packed RAW10/RAW12 rows, see [`Packing`]) with its [`DngMetadata`]
//!   (black level per CFA cell, white level, as-shot neutral, colour calibration at up to two
//!   illuminants, lens shading as `GainMap` opcodes, exposure, ISO, capture time, camera
//!   names, an sRGB preview) to a little-endian DNG: IFD 0 holds the preview (or the raw
//!   image when there is none), a SubIFD the raw image, an EXIF IFD the exposure.
//! * [`read_dng`]: what a calibration tool needs from a DNG, this writer's or a camera's:
//!   raw samples (uncompressed at any bit depth, or lossless JPEG, in strips or tiles), CFA
//!   layout, black and white levels, as-shot neutral, colour and forward matrices with their
//!   illuminants, analogue balance, baseline exposure, exposure time, ISO, names, opcode lists.
//! * [`color`]: the matrices: DNG `ColorMatrix` (XYZ → camera) and `ForwardMatrix`
//!   (white-balanced camera → XYZ D50) from a camera → sRGB colour correction matrix and the
//!   camera's neutral under an illuminant, and back.
//! * [`opcode`]: DNG opcode lists, `GainMap` (lens shading per CFA channel) encoded and
//!   decoded.
//!
//! # Colour
//!
//! A tuned ISP (Raspberry Pi tunings, Styx's) white-balances the camera's RGB `c` with gains
//! `g = 1 / n` (`n`: the camera's response to grey under the light, green 1) and maps it to
//! linear sRGB with a CCM `M` calibrated at that light's colour temperature:
//! `s = M · diag(1/n) · c`. The CCM maps the light's white to sRGB white (rows sum to 1), so it
//! includes the chromatic adaptation to D65. In XYZ, with `A` the sRGB → XYZ (D65) matrix and
//! `B` Bradford's adaptation from D65 to the light's white `W`, the colour the camera saw is
//! `XYZ = B · A · M · diag(1/n) · c`. DNG's `ColorMatrix` maps XYZ to the camera, so
//!
//! ```text
//! ColorMatrix = k · (B · A · M · diag(1/n))⁻¹ = k · diag(n) · M⁻¹ · A⁻¹ · B⁻¹
//! ```
//!
//! with `k` making the largest component of `ColorMatrix · W` (the camera's neutral for the
//! light, `n` up to scale) 1, as DNG asks. `ForwardMatrix` maps white-balanced camera RGB to
//! XYZ D50: `Bradford(D65 → D50) · A · M`, rows scaled so `(1, 1, 1)` maps to D50's white.
//! Calibrating at two lights (here standard illuminant A, 2856 K, and D65, 6504 K: the CCM and
//! the neutral at each come from the tuning's CCMs and AWB curve) lets raw converters
//! interpolate for the shot's light, which they find from `AsShotNeutral` (`1 / g`).
//! [`color::ccm_from_color_matrix`] goes back from a `ColorMatrix` to the CCM and neutral.

pub mod color;
pub mod ljpeg;
pub mod opcode;
mod raw;
mod reader;
mod tiff;
mod tiff_read;
mod writer;

pub use color::{Illuminant, Matrix3};
pub use opcode::{GainMap, Opcode};
pub use raw::{CfaPattern, Packing, RawImage, SampleLayout, unpack};
pub use reader::{Calibration, DngFile, read_dng};
pub use writer::{ColorCalibration, DngMetadata, Preview, write_dng};

/// Errors writing or reading DNG files.
#[derive(Debug, thiserror::Error)]
pub enum DngError {
    /// The input image or metadata is inconsistent (sizes, bit depths).
    #[error("invalid input: {0}")]
    Invalid(String),
    /// The file is not a TIFF/DNG or is damaged.
    #[error("malformed file: {0}")]
    Malformed(String),
    /// The file uses something this reader does not decode.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Writing failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Shorthand for results of this crate.
pub type Result<T> = std::result::Result<T, DngError>;

pub(crate) fn invalid(msg: impl Into<String>) -> DngError {
    DngError::Invalid(msg.into())
}

pub(crate) fn malformed(msg: impl Into<String>) -> DngError {
    DngError::Malformed(msg.into())
}
