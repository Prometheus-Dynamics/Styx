//! Regions of interest and overviews. A native camera's PiSP crops the region at full
//! resolution (its main output) and scales the whole frame down for the overview (its second
//! output), in the pass that makes the frame; elsewhere luma frames are views of the region
//! (an MJPEG decode skips the rows below it) and the overview is the uncropped frame.

use styx_core::prelude::*;

use crate::capture_api::StyxConfig;
use crate::prelude::{Mode, ProbedBackend};

use super::routes::{Candidate, native_isp_outputs};
use super::{
    FramePlan, FrameRequest, Hardware, PlanStep, PyramidSource, Route, StepCost, StepExecution,
    StepKind, Unmet,
};

/// How a region of interest reaches the frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum RoiCrop {
    /// The ISP crops it: frames hold only the region, at full resolution, in any format
    /// (sized to it, rounded out to even pixels); a new region applies from the next frame the
    /// ISP processes.
    Isp,
    /// Frames are views of the region in the captured (or decoded) frame; luma frames only.
    View,
}

/// How a plan delivers its request's region of interest and overview.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Region {
    /// How the region is applied; `None` without one, or where it is not (an unmet part).
    pub roi: Option<RoiCrop>,
    /// The overview's size, and whether the ISP's second output makes it.
    pub overview: Option<((u32, u32), bool)>,
}

impl Region {
    /// The ISP crops (and the capture is set up for it), whether or not a region is set yet.
    pub(crate) fn isp(&self) -> bool {
        self.roi == Some(RoiCrop::Isp) || self.overview.is_some_and(|(_, isp)| isp)
    }

    /// The overview the ISP makes, at its size.
    pub(crate) fn isp_overview(&self) -> Option<(u32, u32)> {
        self.overview.filter(|(_, isp)| *isp).map(|(size, _)| size)
    }
}

/// Whether the PiSP crops and makes the overview for `req` on this route: a native camera's
/// processed mode, frames passed through at the mode's size, the second output not needed
/// for a hardware-only pyramid.
pub(crate) fn isp_possible(
    backend: &ProbedBackend,
    mode: &Mode,
    route: &Route,
    req: &FrameRequest,
    isp_output: Option<(u32, u32)>,
) -> bool {
    (req.roi.is_some() || req.overview.is_some())
        && native_isp_outputs(backend, mode)
        && isp_output.is_none()
        && matches!(route, Route::Direct | Route::LumaView)
        && !matches!(req.hardware, Hardware::Off)
        && !req
            .pyramid
            .is_some_and(|p| p.levels > 0 && p.source == PyramidSource::HardwareOnly)
}

/// `req` with its pyramid box-filtered from the frames: the ISP's second output makes the
/// overview, and a crop's pyramid must be of the crop.
pub(crate) fn software_pyramid(req: &FrameRequest) -> FrameRequest {
    let mut req = req.clone();
    if let Some(p) = &mut req.pyramid {
        p.source = PyramidSource::Software;
    }
    req
}

/// The overview's size for about `wanted`: the smallest even size with the frame's aspect
/// ratio covering it, at least 16x16 and at most the frame.
pub(crate) fn overview_size(frame: (u32, u32), wanted: (u32, u32)) -> (u32, u32) {
    let scale = (f64::from(wanted.0) / f64::from(frame.0))
        .max(f64::from(wanted.1) / f64::from(frame.1))
        .min(1.0);
    let side = |full: u32| {
        ((f64::from(full) * scale).ceil() as u32)
            .next_multiple_of(2)
            .clamp(16.min(full), full & !1)
    };
    (side(frame.0), side(frame.1))
}

/// The end of the overview step's detail.
const ISP_OVERVIEW: &str = " overview of the whole frame from the ISP's second output";

/// How `req`'s region and overview reach frames of `frame` size (before any crop) on `route`,
/// adding their steps and notes. Fails when an overview is asked of encoded frames.
pub(crate) fn plan(
    req: &FrameRequest,
    route: &Route,
    isp: bool,
    frame: (u32, u32),
    steps: &mut Vec<PlanStep>,
    notes: &mut Vec<String>,
) -> Result<Region, String> {
    let encoded = matches!(route, Route::Encode { .. });
    if encoded && req.overview.is_some() {
        return Err("an overview needs uncompressed frames".into());
    }
    let luma = matches!(req.format, OutputFormat::Luma);
    let roi = match req.roi {
        None => None,
        Some(_) if isp => Some(RoiCrop::Isp),
        Some(_) if luma && !encoded => Some(RoiCrop::View),
        Some(_) => {
            notes.push(if encoded {
                "the region of interest is not applied to encoded frames".into()
            } else {
                "the region of interest is not applied: only luma frames are cropped on this route"
                    .into()
            });
            None
        }
    };
    match roi {
        Some(RoiCrop::Isp) => steps.push(PlanStep {
            kind: StepKind::Crop,
            execution: StepExecution::Hardware,
            detail: "region of interest cropped by the ISP at full resolution".into(),
            cost: StepCost::ZERO,
        }),
        Some(RoiCrop::View) => steps.push(PlanStep {
            kind: StepKind::Crop,
            execution: StepExecution::ZeroCopy,
            detail: match route {
                Route::Decode { decoder, .. }
                    if decoder.descriptor().impl_name == "turbojpeg-luma" =>
                {
                    "region of interest; the JPEG decoder skips rows below it".into()
                }
                _ => "region of interest view".into(),
            },
            cost: StepCost::ZERO,
        }),
        None => {}
    }
    let overview = req.overview.map(|wanted| {
        if isp {
            let (w, h) = overview_size(frame, wanted);
            steps.push(PlanStep {
                kind: StepKind::Scale,
                execution: StepExecution::Hardware,
                detail: format!("{w}x{h}{ISP_OVERVIEW}"),
                cost: StepCost::ZERO,
            });
            ((w, h), true)
        } else {
            notes.push(format!(
                "the overview is the uncropped {}x{} frame (no ISP here to scale it)",
                frame.0, frame.1
            ));
            (frame, false)
        }
    });
    Ok(Region { roi, overview })
}

impl FramePlan {
    /// `config` with the capture set up for this plan's ISP crop and overview: the initial
    /// region, and the second output making the overview.
    pub(crate) fn region_config(&self, mut config: StyxConfig) -> StyxConfig {
        if self.region.roi == Some(RoiCrop::Isp)
            && let Some(roi) = self.request.roi
        {
            config = config.native_crop(roi);
        }
        if let Some((width, height)) = self.region.isp_overview() {
            config = config.native_overview(width, height);
        }
        config
    }
}

impl Candidate<'_> {
    /// This candidate with the region and overview applied after the capture, for a capture
    /// shared with other consumers (one ISP crop cannot serve them all).
    pub(crate) fn without_isp_region(&mut self, req: &FrameRequest) {
        if !self.region.isp() {
            return;
        }
        self.steps
            .retain(|s| s.kind != StepKind::Crop && !s.detail.ends_with(ISP_OVERVIEW));
        let frame = self.delivered_size();
        if let Ok(region) = plan(
            req,
            &self.route,
            false,
            frame,
            &mut self.steps,
            &mut self.notes,
        ) {
            self.region = region;
        }
        self.notes.push(
            "other consumers share the capture, so its ISP does not crop for this one".into(),
        );
    }

    /// What of `req` this candidate's frames do not meet.
    pub(crate) fn unmet(&self, req: &FrameRequest) -> Vec<Unmet> {
        let mut unmet = super::delivered::unmet(req, self.delivered_size());
        if req.roi.is_some() && self.region.roi.is_none() {
            unmet.push(Unmet::Roi);
        }
        if let (Some(wanted), Some((delivered, _))) = (req.overview, self.region.overview)
            && delivered.0 > wanted.0
            && delivered.1 > wanted.1
        {
            unmet.push(Unmet::Overview { wanted, delivered });
        }
        unmet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overviews_keep_the_frame_aspect_and_cover_the_size() {
        assert_eq!(overview_size((1280, 800), (320, 200)), (320, 200));
        assert_eq!(overview_size((1280, 720), (320, 320)), (570, 320));
        assert_eq!(overview_size((1280, 800), (5, 5)), (16, 16));
        assert_eq!(overview_size((1280, 800), (4000, 4000)), (1280, 800));
    }
}
