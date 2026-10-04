//! What a plan's frames are, known before the first one arrives, and what of the request they do
//! not meet.

use std::fmt;

use styx_core::prelude::*;

use super::{FramePlan, FrameRequest, RoiCrop, Route};

/// A part of a request a plan does not meet. Frames still come, as [`Delivered`] describes,
/// unless the request is [`FrameRequest::strict`], which turns any of these into a planning
/// error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Unmet {
    /// No route scales to the size asked for ([`FrameRequest::size`]): frames arrive larger in
    /// both dimensions. (Covering it in one dimension and keeping the camera's aspect ratio in
    /// the other is meeting it.)
    Size {
        wanted: (u32, u32),
        delivered: (u32, u32),
    },
    /// The region of interest ([`FrameRequest::roi`]) is not applied: frames are whole (a
    /// route that crops only luma frames, or encoded frames).
    Roi,
    /// The overview ([`FrameRequest::overview`]) is larger in both dimensions than asked.
    Overview {
        wanted: (u32, u32),
        delivered: (u32, u32),
    },
}

impl fmt::Display for Unmet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unmet::Size {
                wanted: (ww, wh),
                delivered: (dw, dh),
            } => write!(f, "frames are {dw}x{dh}, not scaled to {ww}x{wh}"),
            Unmet::Roi => write!(f, "frames are not cropped to the region of interest"),
            Unmet::Overview {
                wanted: (ww, wh),
                delivered: (dw, dh),
            } => write!(f, "the overview is {dw}x{dh}, not scaled to {ww}x{wh}"),
        }
    }
}

/// The parts of `req` that frames of `size` do not meet.
pub(crate) fn unmet(req: &FrameRequest, size: (u32, u32)) -> Vec<Unmet> {
    let mut unmet = Vec::new();
    if let Some(wanted) = req.size
        && size.0 > wanted.0
        && size.1 > wanted.1
    {
        unmet.push(Unmet::Size {
            wanted,
            delivered: size,
        });
    }
    unmet
}

/// What a plan's frames are: format, size, rate and pyramid, and what of the request they do not
/// meet. Known at plan time, so consumers can size buffers and regions once.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Delivered {
    pub format: FourCc,
    /// Width and height of the frames.
    pub size: (u32, u32),
    /// Frames per second the capture runs at, when the plan sets a rate.
    pub fps: Option<f32>,
    /// Pyramid levels attached to each frame (½, ¼, ... as companions).
    pub pyramid_levels: u8,
    /// The deepest of those levels the ISP makes in hardware (the rest are box-filtered on the
    /// CPU); `None` when all are computed on the CPU.
    pub hardware_pyramid_level: Option<u8>,
    /// Inter-coded packets (H.264/H.265): only keyframes stand alone.
    pub inter_coded: bool,
    /// How the region of interest is applied (frames are then the region, and say where it
    /// is in `FrameMeta::crop`); `None` without one, or when it is not applied (see `unmet`).
    /// `size` is the whole frame's.
    pub roi: Option<RoiCrop>,
    /// How each region of interest is applied, region 0 (`roi`) first, then the others
    /// (`FrameRequest::regions`, attached as `CompanionKind::Region` companions); `None` where
    /// one is not applied (see `unmet`). Empty without regions.
    pub regions: Vec<Option<RoiCrop>>,
    /// The size of the whole-frame overview attached to each frame (`FrameLease::overview`).
    pub overview: Option<(u32, u32)>,
    /// The ISP makes the overview (else it is box-filtered from the frame's luma on the CPU).
    pub hardware_overview: bool,
    /// What of the request the frames do not meet (empty: everything is met).
    pub unmet: Vec<Unmet>,
}

impl FramePlan {
    /// The format frames arrive in.
    pub fn output_format(&self) -> FourCc {
        match &self.route {
            Route::Decode { decoder, .. } => decoder.descriptor().output,
            Route::Encode { encoder, .. } => encoder.descriptor().output,
            _ if matches!(self.request.format, OutputFormat::Luma) => FourCc::GREY,
            _ => self.isp_format.unwrap_or(self.mode.format.code),
        }
    }

    /// What this plan's frames are ([`Delivered`]), before the first one arrives.
    pub fn delivered(&self) -> Delivered {
        Delivered {
            format: self.output_format(),
            size: self.output_resolution(),
            fps: self.interval.map(|i| i.fps()),
            pyramid_levels: if matches!(self.route, Route::Encode { .. }) {
                0
            } else {
                self.request.pyramid.map_or(0, |p| p.levels)
            },
            hardware_pyramid_level: self.isp_pyramid_level,
            inter_coded: self.inter_coded(),
            roi: self.region.roi(),
            regions: self.region.crops.clone(),
            overview: self.region.overview.map(|(size, _)| size),
            hardware_overview: self.region.overview.is_some_and(|(_, isp)| isp),
            unmet: self.unmet.clone(),
        }
    }
}
