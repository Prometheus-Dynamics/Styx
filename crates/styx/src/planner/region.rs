//! Regions of interest and overviews. A native camera's PiSP crops regions at full resolution
//! (the main output, its second output, or extra back end passes over the same raw frame) and
//! scales the whole frame down for the overview (its second output), from the raw frame each
//! frame comes from; its software ISP processes only the first region and bins the whole frame
//! for the overview (`region_soft`); a Raspberry Pi ISP through libcamera crops the first per
//! output (`rpi::ScalerCrops`); elsewhere luma frames are views of the regions (an MJPEG decode
//! skips the rows below the first) and the overview is the frame box-filtered down.
//!
//! Which region goes where is priced with [`cost`](super::cost): a view of a frame the capture
//! makes anyway costs nothing; an extra pass costs back end time for the region's pixels and
//! a little CPU; a crop of the main output saves the back end everything outside it, so it is
//! taken whenever no consumer needs the whole frame. Shared captures: `region_shared`.

use styx_core::prelude::*;

use crate::capture_api::{NativeRegion, StyxConfig};
use crate::prelude::{Mode, ProbedBackend};

use super::cost::{self, StepCost};
use super::native::{binned, native_isp, raw_bayer};
use super::region_soft::{SOFT_OVERVIEW, reprice_soft_capture, soft_region_ms};
pub(crate) use super::region_soft::{soft_overview_factor, soft_overview_size};
use super::routes::{Candidate, has_isp_second_output, native_isp_outputs};
use super::{
    FramePlan, FrameRequest, Hardware, PlanStep, PyramidSource, Route, StepExecution, StepKind,
    Unmet,
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
    /// The ISP crops it in the pass that makes the frame (its main or second output): frames
    /// (or region companions) hold only the region, at full resolution, in any format (sized
    /// to it, rounded out to even pixels); a new region applies from the next frame the ISP
    /// processes.
    Isp,
    /// An extra ISP pass over the same raw frame crops it, as [`RoiCrop::Isp`] (a native
    /// camera's PiSP: about 0.05 ms of back end time for 128x128, 0.4 ms for 640x400, on the
    /// frame's path).
    IspPass,
    /// Frames are views of the region in the captured (or decoded) frame; luma frames only.
    View,
}

/// Where the ISP makes a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum IspPlace {
    /// The main output's crop (`OUTPUT_CROP`).
    Main,
    /// The capture's region slot `k` (`NativeIspConfig::regions[k]`, companion index `k + 1`).
    Slot(u8),
}

/// How a plan delivers its request's regions of interest and overview.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Region {
    /// How each region is applied (index 0 is `FrameRequest::roi`; `None`: not applied, an
    /// unmet part).
    pub crops: Vec<Option<RoiCrop>>,
    /// Where the ISP makes each of them (same indices; `None` for views and unapplied ones).
    pub places: Vec<Option<IspPlace>>,
    /// The overview's size, and whether the ISP makes it.
    pub overview: Option<((u32, u32), bool)>,
    /// The software ISP crops: the capture step's cost before it was priced for the region.
    pub full_capture_cost: Option<StepCost>,
    /// The ISP that crops and makes the overview.
    pub isp: Option<IspCrop>,
}

impl Region {
    /// How region 0 is applied.
    pub(crate) fn roi(&self) -> Option<RoiCrop> {
        self.crops.first().copied().flatten()
    }

    /// The ISP crops (and the capture is set up for it), whether or not a region is set yet.
    pub(crate) fn isp(&self) -> bool {
        self.places.iter().any(Option::is_some) || self.isp_overview().is_some()
    }

    /// The overview the ISP makes, at its size.
    pub(crate) fn isp_overview(&self) -> Option<(u32, u32)> {
        self.overview.filter(|(_, isp)| *isp).map(|(size, _)| size)
    }

    /// The ISP crops region 0 in the main output.
    pub(crate) fn main_crop(&self) -> bool {
        self.places.first() == Some(&Some(IspPlace::Main))
    }

    /// The ISP crops region 0 (the main output, or a region slot): frames arrive cropped, and
    /// nothing crops them again.
    pub(crate) fn frames_cropped(&self) -> bool {
        self.places.first().is_some_and(Option::is_some)
    }

    /// The ISP's region slots this plan's regions use, with their region indices.
    pub(crate) fn slots(&self) -> impl Iterator<Item = (usize, u8)> + '_ {
        self.places.iter().enumerate().filter_map(|(i, p)| match p {
            Some(IspPlace::Slot(k)) => Some((i, *k)),
            _ => None,
        })
    }
}

/// Which ISP crops and makes the overview for `req` on this route, if one does: a native
/// camera's processed mode, frames passed through at the mode's size; the PiSP unless the
/// request keeps work off hardware or wants more than one hardware-only pyramid level (one
/// comes from an extra pass of the region); libcamera's Raspberry Pi ISP unless it needs its
/// second output for a hardware-only pyramid; the software ISP (on the CPU) on a mode it
/// processes at full size.
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
    let hardware_only = |levels: u8| {
        req.pyramid
            .is_some_and(|p| p.levels > levels && p.source == PyramidSource::HardwareOnly)
    };
    let hardware = !matches!(req.hardware, Hardware::Off);
    if native_isp_outputs(backend, mode) {
        return (hardware && !hardware_only(1)).then_some(IspCrop::Pisp);
    }
    // libcamera crops to a region's size: one is needed to size the main output.
    if has_isp_second_output(backend) {
        let crops = backend
            .descriptor
            .controls
            .iter()
            .any(|c| c.name == "ScalerCrops");
        return (hardware && !hardware_only(0) && crops && req.roi.is_some())
            .then_some(IspCrop::Libcamera);
    }
    (native_isp(backend) == Some("software")
        && !raw_bayer(mode.format.code)
        && !binned(backend, mode)
        && !super::native::gpu_isp_present())
    .then_some(IspCrop::Software)
}

/// `req` with its pyramid as the ISP's crop allows: the second output makes the overview, and
/// a crop's pyramid must be of the crop, so levels are box-filtered from the frames, unless
/// only hardware will do (one level, from an extra pass of the region) or the pass is cheaper
/// than the box filter (`region`: the region's size). Only the PiSP makes such passes.
pub(crate) fn isp_region_pyramid(
    req: &FrameRequest,
    region: (u32, u32),
    isp: IspCrop,
) -> FrameRequest {
    let mut req = req.clone();
    if let Some(p) = &mut req.pyramid {
        if isp != IspCrop::Pisp {
            p.source = PyramidSource::Software;
            return req;
        }
        let pass = cost::score(cost::pisp_pass(region.0, region.1));
        let boxed = cost::score(StepCost::cpu(
            cost::BOX_LEVEL_MS_PER_MP * cost::megapixels(region.0, region.1),
        ));
        let hardware = match p.source {
            PyramidSource::HardwareOnly => true,
            PyramidSource::PreferHardware => pass < boxed,
            PyramidSource::Software => false,
        };
        if !hardware {
            p.source = PyramidSource::Software;
        }
    }
    req
}

/// The overview's size for about `wanted`: the smallest even size with the frame's aspect
/// ratio covering it, at least 16x16 (the ISP's smallest output) and at most the frame.
pub(crate) fn overview_size(frame: (u32, u32), wanted: (u32, u32)) -> (u32, u32) {
    sized_overview(frame, wanted, 16)
}

/// [`overview_size`] for an overview box-filtered on the CPU: at least 2x2.
pub(crate) fn cpu_overview_size(frame: (u32, u32), wanted: (u32, u32)) -> (u32, u32) {
    sized_overview(frame, wanted, 2)
}

fn sized_overview(frame: (u32, u32), wanted: (u32, u32), min: u32) -> (u32, u32) {
    let scale = (f64::from(wanted.0) / f64::from(frame.0))
        .max(f64::from(wanted.1) / f64::from(frame.1))
        .min(1.0);
    let side = |full: u32| {
        ((f64::from(full) * scale).ceil() as u32)
            .next_multiple_of(2)
            .clamp(min.min(full), full & !1)
    };
    (side(frame.0), side(frame.1))
}

/// The end of the overview step's detail.
pub(crate) const ISP_OVERVIEW: &str = " overview of the whole frame from the ISP's second output";

/// The overview of a `frame`-sized frame in `code` for about `wanted`: from the ISP, else
/// box-filtered from the frame's luma, else (no luma plane) the uncropped frame itself.
fn cpu_or_isp_overview(
    isp: bool,
    frame: (u32, u32),
    code: FourCc,
    wanted: (u32, u32),
    notes: &mut Vec<String>,
) -> ((u32, u32), bool) {
    if isp {
        return (overview_size(frame, wanted), true);
    }
    if has_luma(code) {
        notes.push(format!(
            "the overview is box-filtered from the {}x{} frame's luma (no ISP here to scale it)",
            frame.0, frame.1
        ));
        (cpu_overview_size(frame, wanted), false)
    } else {
        notes.push(format!(
            "the overview is the uncropped {}x{} frame ({code} has no luma plane to box-filter)",
            frame.0, frame.1
        ));
        (frame, false)
    }
}

/// Frames in `code` have a luma plane (grey, planar or semi-planar YUV of 8 bits).
pub(crate) fn has_luma(code: FourCc) -> bool {
    if matches!(code, FourCc::GREY | FourCc::R8) {
        return true;
    }
    let info = code.layout_info();
    matches!(
        info.storage,
        FrameStorageKind::Planar | FrameStorageKind::SemiPlanar
    ) && !matches!(
        info.bit_depth,
        BitDepth::U16 | BitDepth::U16x3 | BitDepth::F32
    )
}

/// The overview step: from the ISP's second output, box-filtered from `frame` (`scaled`), or
/// the frame itself.
fn overview_step(size: (u32, u32), isp: bool, frame: (u32, u32), scaled: bool) -> PlanStep {
    let (w, h) = size;
    if !scaled {
        return PlanStep {
            kind: StepKind::Scale,
            execution: StepExecution::ZeroCopy,
            detail: format!("{w}x{h} overview of the whole frame: the uncropped frame"),
            cost: StepCost::ZERO,
        };
    }
    if isp {
        PlanStep {
            kind: StepKind::Scale,
            execution: StepExecution::Hardware,
            detail: format!("{w}x{h}{ISP_OVERVIEW}"),
            cost: StepCost::ZERO,
        }
    } else {
        PlanStep {
            kind: StepKind::Scale,
            execution: StepExecution::Cpu,
            detail: format!("{w}x{h} overview of the whole frame, box-filtered from its luma"),
            cost: StepCost::cpu(cost::BOX_LEVEL_MS_PER_MP * cost::megapixels(frame.0, frame.1)),
        }
    }
}

/// The steps a plan's regions take: region 0, then the others by how they are made.
pub(crate) fn region_steps(
    req: &FrameRequest,
    route: &Route,
    (crops, isp): (&[Option<RoiCrop>], Option<IspCrop>),
    steps: &mut Vec<PlanStep>,
) {
    let rects = req.all_regions();
    let pass =
        |r: Option<&FrameRect>| r.map_or(StepCost::ZERO, |r| cost::pisp_pass(r.width, r.height));
    match crops.first().copied().flatten() {
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
        Some(RoiCrop::IspPass) => steps.push(PlanStep {
            kind: StepKind::Crop,
            execution: StepExecution::Hardware,
            detail: "region of interest cropped by an extra ISP pass at full resolution".into(),
            cost: pass(rects.first()),
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
    for (kind, execution, what) in [
        (
            RoiCrop::Isp,
            StepExecution::Hardware,
            "cropped by the ISP's second output",
        ),
        (
            RoiCrop::IspPass,
            StepExecution::Hardware,
            "cropped by extra ISP passes over the same raw frame",
        ),
        (
            RoiCrop::View,
            StepExecution::ZeroCopy,
            "as views of the frame",
        ),
    ] {
        let of_kind: Vec<usize> = (1..crops.len())
            .filter(|&i| crops[i] == Some(kind))
            .collect();
        if of_kind.is_empty() {
            continue;
        }
        let cost = match kind {
            RoiCrop::IspPass => of_kind
                .iter()
                .map(|&i| pass(rects.get(i)))
                .fold(StepCost::ZERO, |a, b| a + b),
            _ => StepCost::ZERO,
        };
        let n = of_kind.len();
        steps.push(PlanStep {
            kind: StepKind::Crop,
            execution,
            detail: format!("{n} more region{} {what}", if n > 1 { "s" } else { "" }),
            cost,
        });
    }
}

/// How `req`'s regions and overview reach frames of `frame` size in `code` (before any crop)
/// on `route` for a consumer alone on its capture, `isp` cropping (the PiSP: region 0 in the
/// main output, the others in extra passes; the software ISP or libcamera: region 0 only),
/// adding their steps and notes. Fails when an overview is asked of encoded frames.
pub(crate) fn plan(
    req: &FrameRequest,
    route: &Route,
    isp: Option<IspCrop>,
    (frame, code): ((u32, u32), FourCc),
    steps: &mut Vec<PlanStep>,
    notes: &mut Vec<String>,
) -> Result<Region, String> {
    let encoded = matches!(route, Route::Encode { .. });
    if encoded && req.overview.is_some() {
        return Err("an overview needs uncompressed frames".into());
    }
    let luma = matches!(req.format, OutputFormat::Luma);
    let mut crops = Vec::new();
    let mut places = Vec::new();
    for i in 0..req.all_regions().len() {
        let (crop, place) = match (isp, i) {
            (Some(_), 0) => (Some(RoiCrop::Isp), Some(IspPlace::Main)),
            (Some(IspCrop::Pisp), i) => (Some(RoiCrop::IspPass), Some(IspPlace::Slot(i as u8 - 1))),
            _ if luma && !encoded => (Some(RoiCrop::View), None),
            _ => (None, None),
        };
        crops.push(crop);
        places.push(place);
    }
    if crops.iter().any(Option::is_none) {
        notes.push(if encoded {
            "the region of interest is not applied to encoded frames".into()
        } else {
            "the region of interest is not applied: only luma frames are cropped on this route"
                .into()
        });
    }
    region_steps(req, route, (&crops, isp), steps);
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
            return ((w, h), true);
        }
        let (size, made) = cpu_or_isp_overview(isp.is_some(), frame, code, wanted, notes);
        steps.push(overview_step(size, made, frame, size != frame || made));
        (size, made)
    });
    let full_capture_cost = (isp == Some(IspCrop::Software))
        .then(|| {
            let ms = soft_region_ms(req, frame, factor);
            let step = steps.iter_mut().find(|s| s.kind == StepKind::Capture)?;
            Some(reprice_soft_capture(step, frame, ms))
        })
        .flatten();
    Ok(Region {
        crops,
        places,
        overview,
        full_capture_cost,
        isp,
    })
}

impl FramePlan {
    /// `config` with the capture set up for this plan's ISP crops and overview: the first
    /// regions, and the ISP making the overview.
    pub(crate) fn region_config(&self, mut config: StyxConfig) -> StyxConfig {
        let rects = self.request.all_regions();
        let roi = self.request.roi.filter(|_| self.region.main_crop());
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
        for (i, k) in self.region.slots() {
            if let Some(slot) = config.backends.native.regions.get_mut(usize::from(k)) {
                *slot = Some(NativeRegion {
                    rect: rects.get(i).copied(),
                    format: None,
                });
            }
        }
        if let Some((width, height)) = self.region.isp_overview() {
            config = config.native_overview(width, height);
        }
        config
    }
}

impl Candidate<'_> {
    /// This candidate with its regions as `crops` and `places` (a shared capture's
    /// assignment) and its overview from the ISP at `overview` (`None`: box-filtered), steps
    /// and cost to match.
    pub(crate) fn set_regions(
        &mut self,
        req: &FrameRequest,
        crops: Vec<Option<RoiCrop>>,
        places: Vec<Option<IspPlace>>,
        overview: Option<(u32, u32)>,
    ) {
        let before: StepCost = self
            .steps
            .iter()
            .filter(|s| is_region_step(s))
            .map(|s| s.cost)
            .fold(StepCost::ZERO, |a, b| a + b);
        self.steps.retain(|s| !is_region_step(s));
        region_steps(
            req,
            &self.route,
            (&crops, Some(IspCrop::Pisp)),
            &mut self.steps,
        );
        let frame = self.delivered_size();
        let code = super::routes::route_output(&self.route, &self.mode);
        let mut notes = Vec::new();
        let overview = req.overview.map(|wanted| {
            let (size, isp) = match overview {
                Some(size) => (size, true),
                None => cpu_or_isp_overview(false, frame, code, wanted, &mut notes),
            };
            self.steps
                .push(overview_step(size, isp, frame, size != frame || isp));
            (size, isp)
        });
        self.notes.extend(notes);
        let after: StepCost = self
            .steps
            .iter()
            .filter(|s| is_region_step(s))
            .map(|s| s.cost)
            .fold(StepCost::ZERO, |a, b| a + b);
        self.total = StepCost {
            latency_ms: self.total.latency_ms - before.latency_ms + after.latency_ms,
            cpu_ms: self.total.cpu_ms - before.cpu_ms + after.cpu_ms,
        };
        self.region = Region {
            crops,
            places,
            overview,
            full_capture_cost: None,
            isp: Some(IspCrop::Pisp),
        };
    }

    /// This candidate with its regions and overview made after the capture, for a capture
    /// shared with other consumers whose ISP crops one region only (the software ISP,
    /// libcamera).
    pub(crate) fn without_isp_region(&mut self, req: &FrameRequest) {
        if !self.region.isp() {
            return;
        }
        self.steps
            .retain(|s| !is_region_step(s) && !s.detail.contains(SOFT_OVERVIEW));
        if let Some(full) = self.region.full_capture_cost
            && let Some(step) = self.steps.iter_mut().find(|s| s.kind == StepKind::Capture)
        {
            step.cost = full;
        }
        let frame = (
            self.delivered_size(),
            super::routes::route_output(&self.route, &self.mode),
        );
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
        let regions = req.all_regions().len();
        if regions > 0
            && (self.region.crops.len() < regions || self.region.crops.iter().any(Option::is_none))
        {
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

/// A step [`region_steps`] or the overview made.
fn is_region_step(s: &PlanStep) -> bool {
    s.kind == StepKind::Crop || s.detail.contains(" overview of the whole frame")
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
    fn a_pass_pyramid_only_when_hardware_is_required() {
        // At the CM5's costs a box filter beats an extra pass for any region size.
        let req = FrameRequest::default().pyramid(1);
        for size in [(64, 64), (640, 400), (4056, 3040)] {
            let p = isp_region_pyramid(&req, size, IspCrop::Pisp)
                .pyramid
                .unwrap();
            assert_eq!(p.source, PyramidSource::Software, "{size:?}");
        }
        let hw = req.clone().pyramid_source(PyramidSource::HardwareOnly);
        let p = isp_region_pyramid(&hw, (640, 400), IspCrop::Pisp)
            .pyramid
            .unwrap();
        assert_eq!(p.source, PyramidSource::HardwareOnly);
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
