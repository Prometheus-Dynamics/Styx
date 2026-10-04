//! Regions of interest and overviews. A native camera's PiSP crops the region at full
//! resolution (its main output) and scales the whole frame down for the overview (its second
//! output), in the pass that makes the frame; its software ISP processes only the region and
//! bins the whole frame for the overview (with the frame's statistics), so a small region costs
//! a small part of a frame; elsewhere luma frames are views of the region (an MJPEG decode skips
//! the rows below it) and the overview is the uncropped frame.

use styx_core::prelude::*;

use crate::capture_api::StyxConfig;
use crate::prelude::{Mode, ProbedBackend};

use super::cost;
use super::native::{binned, native_isp, raw_bayer};
use super::routes::{Candidate, has_isp_second_output, native_isp_outputs};
use super::{
    FramePlan, FrameRequest, Hardware, PlanStep, PyramidSource, Route, StepCost, StepExecution,
    StepKind, Unmet,
};

/// The ISP that crops a plan's region and makes its overview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IspCrop {
    /// A native camera's PiSP: back end crop and second output.
    Pisp,
    /// A native camera's software ISP: the region processed alone, the overview binned.
    Software,
    /// A Raspberry Pi ISP through libcamera: per-output crops (`rpi::ScalerCrops`), the main
    /// output at the first region's size, the second the whole frame.
    Libcamera,
}

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
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Region {
    /// How the region is applied; `None` without one, or where it is not (an unmet part).
    pub roi: Option<RoiCrop>,
    /// The overview's size, and whether the ISP makes it.
    pub overview: Option<((u32, u32), bool)>,
    /// The software ISP crops: the capture step's cost before it was priced for the region.
    pub full_capture_cost: Option<StepCost>,
    /// The ISP that crops and makes the overview.
    pub isp: Option<IspCrop>,
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

/// Which ISP crops and makes the overview for `req` on this route, if one does: a native
/// camera's processed mode, frames passed through at the mode's size; the PiSP unless the
/// request keeps work off hardware or needs its second output for a hardware-only pyramid;
/// the software ISP (on the CPU) on a mode it processes at full size.
pub(crate) fn isp_possible(
    backend: &ProbedBackend,
    mode: &Mode,
    route: &Route,
    req: &FrameRequest,
    isp_output: Option<(u32, u32)>,
) -> Option<IspCrop> {
    if (req.roi.is_none() && req.overview.is_none())
        || isp_output.is_some()
        || !matches!(route, Route::Direct | Route::LumaView)
    {
        return None;
    }
    let hardware = !matches!(req.hardware, Hardware::Off)
        && !req
            .pyramid
            .is_some_and(|p| p.levels > 0 && p.source == PyramidSource::HardwareOnly);
    if native_isp_outputs(backend, mode) {
        return hardware.then_some(IspCrop::Pisp);
    }
    // libcamera crops to a region's size: one is needed to size the main output.
    if has_isp_second_output(backend) {
        let crops = backend
            .descriptor
            .controls
            .iter()
            .any(|c| c.name == "ScalerCrops");
        return (hardware && crops && req.roi.is_some()).then_some(IspCrop::Libcamera);
    }
    (native_isp(backend) == Some("software")
        && !raw_bayer(mode.format.code)
        && !binned(backend, mode)
        && !super::native::gpu_isp_present())
    .then_some(IspCrop::Software)
}

/// The binning factor of the software ISP's overview of about `wanted` from a `frame`-sized
/// frame: the largest even factor whose picture still covers `wanted` in both dimensions (at
/// least 2).
pub(crate) fn soft_overview_factor(frame: (u32, u32), wanted: (u32, u32)) -> u32 {
    let fit = |full: u32, want: u32| full / want.max(1);
    (fit(frame.0, wanted.0).min(fit(frame.1, wanted.1)) & !1).max(2)
}

/// The size of the software ISP's overview binned by `factor` (`SoftIsp::binned_size`).
pub(crate) fn soft_overview_size(frame: (u32, u32), factor: u32) -> (u32, u32) {
    ((frame.0 / factor) & !1, (frame.1 / factor) & !1)
}

/// The software ISP's milliseconds per frame for `req`'s region (the whole frame without one;
/// a region set later is priced as the whole frame) and its overview binned by `factor` (else
/// the statistics on their own) from `frame`.
fn soft_region_ms(req: &FrameRequest, frame: (u32, u32), factor: Option<u32>) -> f32 {
    let mp = |w: u32, h: u32| cost::megapixels(w, h);
    let region = req
        .roi
        .and_then(|r| r.clipped_to(frame.0, frame.1))
        .map_or(mp(frame.0, frame.1), |r| mp(r.width, r.height));
    let raw = mp(frame.0, frame.1);
    let overview = match factor {
        Some(f) => {
            let (w, h) = soft_overview_size(frame, f);
            cost::SOFTISP_BIN_FRONT_MS_PER_RAW_MP * raw * 2.0 / f as f32
                + cost::SOFTISP_BIN_OUT_MS_PER_MP * mp(w, h)
                + cost::SOFTISP_STATS_MS_PER_RAW_MP * raw
        }
        None => {
            (cost::SOFTISP_STATS_MS_PER_RAW_MP + cost::SOFTISP_BIN_FRONT_MS_PER_RAW_MP / 4.0) * raw
        }
    };
    cost::SOFTISP_REGION_MS_PER_MP * region + overview
}

/// `step` (the software ISP's capture step) priced for `isp_ms` of ISP work instead of the
/// whole frame's; returns its cost before.
fn reprice_soft_capture(step: &mut PlanStep, frame: (u32, u32), isp_ms: f32) -> StepCost {
    let before = step.cost;
    let full = cost::SOFTISP_MS_PER_MP * cost::megapixels(frame.0, frame.1);
    let threads = cost::default_softisp_threads();
    let factor = cost::softisp_latency_ms_per_mp(threads) / cost::SOFTISP_MS_PER_MP;
    let saved = (full - isp_ms).max(0.0);
    // The helper threads' overhead shrinks with the work they share.
    let helpers = if threads > 1 {
        cost::SOFTISP_THREADS_CPU_MS * saved / full.max(f32::EPSILON)
    } else {
        0.0
    };
    step.cost = StepCost::offloaded(
        (before.latency_ms - saved * factor).max(0.0),
        (before.cpu_ms - saved - helpers).max(0.0),
    );
    before
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
/// The software ISP's overview step's detail, after its size.
const SOFT_OVERVIEW: &str = " overview of the whole frame from the software ISP";

/// How `req`'s region and overview reach frames of `frame` size (before any crop) on `route`,
/// adding their steps and notes. Fails when an overview is asked of encoded frames.
pub(crate) fn plan(
    req: &FrameRequest,
    route: &Route,
    isp: Option<IspCrop>,
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
        Some(_) if isp.is_some() => Some(RoiCrop::Isp),
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
        Some(RoiCrop::Isp) if isp == Some(IspCrop::Software) => steps.push(PlanStep {
            kind: StepKind::Crop,
            execution: StepExecution::Cpu,
            detail: "region of interest processed alone by the software ISP at full resolution \
                     (priced in the capture step)"
                .into(),
            cost: StepCost::ZERO,
        }),
        Some(RoiCrop::Isp) if isp == Some(IspCrop::Libcamera) => steps.push(PlanStep {
            kind: StepKind::Crop,
            execution: StepExecution::Hardware,
            detail: "region of interest cropped by the ISP (rpi::ScalerCrops) to the first \
                     region's size; a new region shows 2-3 frames after it is set"
                .into(),
            cost: StepCost::ZERO,
        }),
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
    let mut factor = None;
    let overview = req.overview.map(|wanted| {
        if isp == Some(IspCrop::Software) {
            let f = soft_overview_factor(frame, wanted);
            factor = Some(f);
            let (w, h) = soft_overview_size(frame, f);
            steps.push(PlanStep {
                kind: StepKind::Scale,
                execution: StepExecution::Cpu,
                detail: format!(
                    "{w}x{h}{SOFT_OVERVIEW}, binned 1/{f}, with the frame's statistics (priced \
                     in the capture step)"
                ),
                cost: StepCost::ZERO,
            });
            ((w, h), true)
        } else if isp.is_some() {
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
    let full_capture_cost = (isp == Some(IspCrop::Software))
        .then(|| {
            let ms = soft_region_ms(req, frame, factor);
            let step = steps.iter_mut().find(|s| s.kind == StepKind::Capture)?;
            Some(reprice_soft_capture(step, frame, ms))
        })
        .flatten();
    Ok(Region {
        roi,
        overview,
        full_capture_cost,
        isp,
    })
}

impl FramePlan {
    /// `config` with the capture set up for this plan's ISP crop and overview: the initial
    /// region, and the second output making the overview.
    pub(crate) fn region_config(&self, mut config: StyxConfig) -> StyxConfig {
        let roi = self
            .request
            .roi
            .filter(|_| self.region.roi == Some(RoiCrop::Isp));
        if self.region.isp == Some(IspCrop::Libcamera) {
            let Some(roi) = roi else { return config };
            // The main output at the region's size (even, at least 16x16, in the frame).
            let fit = crate::capture_api::fit_crop(roi, self.output_resolution()).unwrap_or(roi);
            return config
                .libcamera_output_size(fit.width, fit.height)
                .libcamera_crop(fit, self.region.isp_overview());
        }
        if let Some(roi) = roi {
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
        self.steps.retain(|s| {
            s.kind != StepKind::Crop
                && !s.detail.ends_with(ISP_OVERVIEW)
                && !s.detail.contains(SOFT_OVERVIEW)
        });
        if let Some(full) = self.region.full_capture_cost
            && let Some(step) = self.steps.iter_mut().find(|s| s.kind == StepKind::Capture)
        {
            step.cost = full;
        }
        let frame = self.delivered_size();
        if let Ok(region) = plan(
            req,
            &self.route,
            None,
            frame,
            &mut self.steps,
            &mut self.notes,
        ) {
            self.region = region;
        }
        self.total = self
            .steps
            .iter()
            .fold(StepCost::ZERO, |sum, s| sum + s.cost);
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

    #[test]
    fn software_overviews_bin_by_the_largest_even_factor_covering_the_size() {
        assert_eq!(soft_overview_factor((1280, 800), (320, 200)), 4);
        assert_eq!(soft_overview_size((1280, 800), 4), (320, 200));
        // 1280x720 at 320x200: 1/4 would be 180 rows.
        assert_eq!(soft_overview_factor((1280, 720), (320, 200)), 2);
        assert_eq!(soft_overview_factor((1280, 800), (100, 100)), 8);
        assert_eq!(soft_overview_size((1280, 800), 8), (160, 100));
        assert_eq!(soft_overview_factor((1280, 800), (4000, 4000)), 2);
    }
}
