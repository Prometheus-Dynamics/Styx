//! Regions of interest on a shared capture: every consumer's regions from one native PiSP (the
//! software ISP and libcamera crop one region only: shared, their consumers get views).
//!
//! One back end pass per frame makes the main and second outputs; extra passes over the same
//! raw frame make further regions (`NativeIspConfig::regions`). The assignment, priced with
//! [`cost`](super::cost) (from the CM5: an extra pass is ~0.03 ms plus 1.35 ns per region
//! pixel of back end time and ~0.025 ms of CPU; a view of a frame made anyway is free):
//!
//! - When every consumer takes regions (none needs the whole frame), the first consumer's
//!   first region is the main output's crop: the frame's own pass reads only around it
//!   (2.3 ms of back end time for 1280x800 with temporal denoise, about 0.1 ms for 128x128).
//! - Otherwise the main output is the whole frame, and a luma consumer's regions are views of
//!   it (free).
//! - Every other region is a region slot of the capture: the second output makes the first
//!   when nothing else takes it and the main output is whole (its pass covers it already; two
//!   crops far apart in one pass cost the tiles between them: 0.51 ms for two 128x128 crops
//!   at opposite corners against 0.27 ms side by side, where a pass each costs 0.05 ms), extra
//!   passes the others, in the main output's format. Beyond the capture's slots, luma regions
//!   fall back to views when the frame is whole, others are unmet.
//! - One overview for the capture, from the second output: the largest asked for.

use styx_core::prelude::*;

use super::cost::{self, StepCost};
use super::region::{IspCrop, IspPlace, RoiCrop};
use super::routes::Candidate;
use super::{FramePlan, FrameRequest};
use crate::capture_api::{MAX_NATIVE_REGIONS, StyxConfig};

/// Assigns `requests`' regions (one per consumer, `candidates` planned for each alone on the
/// shared capture) to the capture's outputs and passes; `second_taken`: a consumer or a pyramid
/// level takes the second output (no ISP overview then). See the
/// [module documentation](self). Fails for a consumer whose hardware-only pyramid would have
/// to follow a region other than the main output's.
pub(crate) fn assign(
    candidates: &mut [Candidate<'_>],
    requests: &[FrameRequest],
    second_taken: bool,
) -> Result<(), String> {
    // The software ISP and libcamera crop one region of the frame: shared, the regions and
    // overviews come from the frames.
    if candidates
        .iter()
        .any(|c| c.region.isp.is_some_and(|isp| isp != IspCrop::Pisp))
    {
        for (candidate, req) in candidates.iter_mut().zip(requests) {
            candidate.without_isp_region(req);
        }
        return Ok(());
    }
    let isp: Vec<bool> = candidates.iter().map(|c| c.region.isp()).collect();
    if !isp.iter().any(|&b| b) {
        return Ok(());
    }
    // Consumers of regions; the others (an overview alone included) take the whole frame.
    let takes_regions: Vec<bool> = isp
        .iter()
        .zip(requests)
        .map(|(&isp, req)| isp && !req.all_regions().is_empty())
        .collect();
    let whole = takes_regions.iter().any(|&b| !b);
    let overview = candidates
        .iter()
        .filter_map(|c| c.region.isp_overview())
        .max_by_key(|&(w, h)| u64::from(w) * u64::from(h))
        .filter(|_| !second_taken);
    let mut main_owner = None;
    let mut slots = 0u8;
    for (i, (candidate, req)) in candidates.iter_mut().zip(requests).enumerate() {
        if !isp[i] {
            continue;
        }
        let luma = matches!(req.format, OutputFormat::Luma);
        // Extra passes write the main output's format: a consumer of the other processed
        // format (on the second output) gets views or nothing.
        let same_format = candidate.isp_format.is_none();
        let mut crops = Vec::new();
        let mut places = Vec::new();
        for (j, rect) in req.all_regions().iter().enumerate() {
            let pass = cost::score(cost::pisp_pass(rect.width, rect.height));
            let view_cheaper = cost::score(StepCost::ZERO) <= pass;
            let (crop, place) = if !whole && j == 0 && main_owner.is_none() && same_format {
                main_owner = Some(i);
                (Some(RoiCrop::Isp), Some(IspPlace::Main))
            } else if luma && whole && view_cheaper {
                (Some(RoiCrop::View), None)
            } else if same_format && usize::from(slots) < MAX_NATIVE_REGIONS {
                slots += 1;
                (Some(RoiCrop::IspPass), Some(IspPlace::Slot(slots - 1)))
            } else if luma && whole {
                (Some(RoiCrop::View), None)
            } else {
                (None, None)
            };
            crops.push(crop);
            places.push(place);
        }
        if candidate.isp_pyramid_level.is_some() && main_owner != Some(i) {
            return Err(format!(
                "consumer {i}: a hardware-only pyramid of ISP-cropped regions needs its region \
                 to be the main output's (the capture to itself)"
            ));
        }
        let own_overview = overview.filter(|_| req.overview.is_some());
        candidate.set_regions(req, crops, places, own_overview);
        candidate
            .notes
            .push("other consumers share the capture: the ISP crops their regions too".into());
    }
    Ok(())
}

/// `config` with the shared capture set up for `consumers`' ISP regions: the main output's
/// crop, the region slots (with their first rectangles) and the overview.
pub(crate) fn region_config(consumers: &[FramePlan], mut config: StyxConfig) -> StyxConfig {
    for plan in consumers {
        config = plan.region_config(config);
    }
    config
}

/// The region slot the shared capture's second output makes, by `config` (its others come from
/// extra passes): consumers' regions there are [`RoiCrop::Isp`], their steps to match.
pub(crate) fn mark_second_output(consumers: &mut [FramePlan], config: &StyxConfig) {
    let Some(k) = config.backends.native.second_output_region() else {
        return;
    };
    for plan in consumers {
        let Some(j) = plan
            .region
            .places
            .iter()
            .position(|p| *p == Some(IspPlace::Slot(k as u8)))
        else {
            continue;
        };
        plan.region.crops[j] = Some(RoiCrop::Isp);
        plan.steps.retain(|s| s.kind != super::StepKind::Crop);
        let route = plan.route.clone();
        let crops = (&plan.region.crops[..], plan.region.isp);
        super::region::region_steps(&plan.request, &route, crops, &mut plan.steps);
    }
}

/// What a shared capture's region set-up is, for consumers joining a running one: where each
/// consumer's regions are made and the overview (not where the regions are: they move).
pub(crate) fn layout_key(consumers: &[FramePlan]) -> String {
    let layout: Vec<_> = consumers
        .iter()
        .filter(|p| p.region.isp())
        .map(|p| (&p.region.places, p.region.isp_overview()))
        .collect();
    format!("{layout:?}")
}
