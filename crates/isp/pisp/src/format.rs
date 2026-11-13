//! The back end's picture formats and the scaling each output can do.
//!
//! [`BACK_END_FORMATS`] are the FourCcs the library's back end writes, each with the library's
//! image format ([`pisp_format`]). They are the codes the reference board's `camera.isp/1` driver
//! takes in `set_output`, so a request for one of them is planned for the PiSP and not refused.
//! [`BACK_END_OUTPUTS`] gives what each output can make from the capture: both outputs are
//! resampled, with factors under 16x either way (`prepare`: "resample factors must be under 16x
//! either way"); output 1 also has a downscaler in front of its resampler, by 2x to 8x. Both
//! therefore make any size from the capture down to a fifteenth of it on each side, the
//! fifteenth keeping the factor under 16x. The PiSP does not upscale a capture here.
//!
//! Colour is not part of a format here: a YUV output takes its colour encoding from the
//! [`crate::Output`] it is built for (see [`crate::Output::encoding`]).

use styx_format::FourCc;
use styx_lemnos::pisp::format::formats;
use styx_lemnos::pisp::uapi::image_format;

use crate::PispError;

/// Each FourCc the back end writes, with the library's image format for it.
pub const FORMATS: [(FourCc, u32); 7] = [
    (FourCc::RGB3, formats::RGB888),
    (FourCc::YU12, formats::YUV420P),
    (FourCc::NV12, formats::NV12),
    (FourCc::NV21, formats::NV21),
    (FourCc::YUYV, formats::YUYV),
    (FourCc::UYVY, formats::UYVY),
    (FourCc::GREY, image_format::BPS_8),
];

/// The FourCcs each back-end output writes, in the library's order (both outputs alike).
pub const BACK_END_FORMATS: [FourCc; 7] = [
    FourCc::RGB3,
    FourCc::YU12,
    FourCc::NV12,
    FourCc::NV21,
    FourCc::YUYV,
    FourCc::UYVY,
    FourCc::GREY,
];

/// What one back-end output can do with a capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputCapability {
    /// The largest integer divisor of the capture an output size can be on each side: an output
    /// of `capture / max_divisor` or larger, never larger than the capture.
    pub max_divisor: u32,
    /// The FourCc picture formats the output writes.
    pub formats: &'static [FourCc],
}

/// Output 0 and output 1: the resampler's range (see the module documentation).
pub const BACK_END_OUTPUTS: [OutputCapability; 2] = [
    OutputCapability {
        max_divisor: 15,
        formats: &BACK_END_FORMATS,
    },
    OutputCapability {
        max_divisor: 15,
        formats: &BACK_END_FORMATS,
    },
];

/// The library's image format for a FourCc the back end writes, or `None`.
pub fn pisp_format(code: FourCc) -> Option<u32> {
    FORMATS.iter().find(|(c, _)| *c == code).map(|(_, f)| *f)
}

/// Whether the FourCc is a YUV picture, which needs a colour space conversion on its output.
pub fn is_yuv(code: FourCc) -> bool {
    matches!(
        code,
        FourCc::NV12 | FourCc::NV21 | FourCc::YU12 | FourCc::YUYV | FourCc::UYVY
    )
}

/// The library's image format for `code`, or the refusal that names it.
pub(crate) fn format_or_refuse(code: FourCc) -> Result<u32, PispError> {
    pisp_format(code).ok_or(PispError::Unsupported(
        "output picture format the back end does not write",
    ))
}
