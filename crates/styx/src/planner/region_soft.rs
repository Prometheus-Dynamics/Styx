//! The native software ISP's region and overview: it processes only the region at full
//! resolution and bins the whole frame for the overview (with the frame's statistics); the
//! capture step is priced for that work.

use super::cost;
use super::{PlanStep, StepCost};

/// The software ISP's overview step's detail, after its size.
pub(crate) const SOFT_OVERVIEW: &str = " overview of the whole frame from the software ISP";

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
pub(crate) fn soft_region_ms(
    req: &super::FrameRequest,
    frame: (u32, u32),
    factor: Option<u32>,
) -> f32 {
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
pub(crate) fn reprice_soft_capture(
    step: &mut PlanStep,
    frame: (u32, u32),
    isp_ms: f32,
) -> StepCost {
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
