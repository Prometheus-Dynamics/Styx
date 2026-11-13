//! Stills from a running camera: one frame as a raw DNG, as a processed picture, or both.
//!
//! A [`StillRequest`] asks for what to make of a frame. [`serve`] makes it from a frame the
//! camera's loop has just taken, with that frame's [`FrameMetadata`] and the ISP settings it is
//! processed with ([`Context`]). A still is the next frame, so the caller serves the request on
//! the frame it takes next and keeps taking frames: this crate never waits, owns a camera or talks
//! to hardware. A camera session or the simulated camera hands out such frames.
//!
//! * **Raw**: the Bayer samples as a DNG 1.4 file ([`styx_dng`]). Black and white levels, the
//!   as-shot neutral from the white balance, the colour calibration (the tuning's CCM and AWB
//!   curve at the two standard illuminants, or the shot's own CCM when the tuning has no AWB
//!   curve), exposure, ISO and lens shading as opcodes come from the frame and its settings.
//! * **Processed**: the frame at full size through the software ISP ([`styx_soft`]), with the
//!   Malvar-He-Cutler demosaic and BT.601 full-range YUV. RGB24 and NV12 are the outputs asked for.
//!
//! Stills are not steady state: a processed picture and a DNG are new allocations, as a still is
//! a one-off. The preview stream's frames are not copied for it; the raw samples are unpacked
//! once into the DNG writer's array.

mod dng;
mod error;
mod processed;

use styx_aaa::Tuning;
use styx_controls::FrameMetadata;
use styx_format::FourCc;
use styx_frame::FrameLease;
use styx_settings::IspSettings;

pub use dng::calibrations;
pub use error::StillError;

/// What to make of one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StillRequest {
    /// The raw frame as a DNG.
    pub raw: bool,
    /// The frame processed at full size into this picture format (`RGB3` or `NV12`, the
    /// software ISP's full-size outputs), or none.
    pub processed: Option<FourCc>,
}

impl StillRequest {
    /// The raw frame only, as a DNG.
    pub const fn raw() -> Self {
        Self {
            raw: true,
            processed: None,
        }
    }

    /// The frame processed to `code` only.
    pub const fn processed(code: FourCc) -> Self {
        Self {
            raw: false,
            processed: Some(code),
        }
    }

    /// The raw frame as a DNG and the frame processed to `code`.
    pub const fn both(code: FourCc) -> Self {
        Self {
            raw: true,
            processed: Some(code),
        }
    }
}

/// What a still is made with, besides the frame: the ISP settings the frame was processed with,
/// the tuning (colour matrices and the AWB curve), and the camera's names for the DNG.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    /// The settings the frame's stream processed it with (white balance, CCM, black level,
    /// digital gain, lens shading, tone curve).
    pub isp: &'a IspSettings,
    /// The camera's tuning. Its CCM and AWB curve give the DNG's colour calibration.
    pub tuning: &'a Tuning,
    /// Camera model (the sensor's name), for the DNG.
    pub model: &'a str,
    /// Unique camera model (names the calibration: sensor and tuning), for the DNG. Empty: the
    /// make and model.
    pub unique_camera_model: &'a str,
}

/// A processed picture: tightly packed planes in the order the format stores them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    /// The format (`RGB3` or `NV12`).
    pub code: FourCc,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The bytes: RGB24 rows, or the Y plane then the interleaved UV plane.
    pub data: Vec<u8>,
}

/// The stills made from one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Still {
    /// The frame's identity (its position in the stream).
    pub sequence: u64,
    /// The frame's `CLOCK_MONOTONIC` timestamp in nanoseconds.
    pub timestamp_ns: u64,
    /// The DNG file, when the request asked for the raw frame.
    pub dng: Option<Vec<u8>>,
    /// The processed picture, when the request asked for one.
    pub picture: Option<Picture>,
}

/// Makes the stills `request` asks for from `frame`, which the camera gave with `meta`.
///
/// The frame is read, not consumed: the caller keeps it, or gives it back to the stream after.
pub fn serve<M>(
    request: &StillRequest,
    frame: &FrameLease<M>,
    meta: &FrameMetadata,
    ctx: &Context<'_>,
) -> Result<Still, StillError> {
    if !request.raw && request.processed.is_none() {
        return Err(StillError::Request("the request asks for nothing".into()));
    }
    let dng = if request.raw {
        Some(dng::write(frame, meta, ctx)?)
    } else {
        None
    };
    let picture = match request.processed {
        Some(code) => Some(processed::picture(frame, ctx.isp, code)?),
        None => None,
    };
    Ok(Still {
        sequence: frame.id(),
        timestamp_ns: frame.timestamp_ns(),
        dng,
        picture,
    })
}
