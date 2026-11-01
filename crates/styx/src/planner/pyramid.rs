//! Pyramid levels: from the ISP's second output (or an extra pass of an ISP-cropped region; on
//! a native PiSP every level, the deeper ones from extra passes), else 2x2 box filters on the
//! CPU, or on the GPU when asked (`PyramidSource::Gpu`, feature `gpu-wgpu`).

use styx_core::prelude::*;

use super::cost::{self, StepCost, megapixels};
use super::request::{FrameRequest, Hardware};
use super::routes::{Route, has_isp_second_output, native_isp_outputs};
use super::{PlanStep, StepExecution, StepKind};
use crate::prelude::{Mode, ProbedBackend};

/// Pyramid steps; returns the deepest level the ISP produces (it makes every level up to it),
/// if any.
pub(crate) fn add_pyramid_steps(
    backend: &ProbedBackend,
    mode: &Mode,
    route: &Route,
    req: &FrameRequest,
    width: u32,
    height: u32,
    steps: &mut Vec<PlanStep>,
) -> Result<Option<u8>, String> {
    let Some(pyramid) = req.pyramid.filter(|p| p.levels > 0) else {
        return Ok(None);
    };
    // A native PiSP mode's second output in the mode's format: NV12, whose Y plane the
    // further levels are box-filtered from.
    let native = native_isp_outputs(backend, mode) && mode.format.code == FourCc::NV12;
    let isp_possible = (has_isp_second_output(backend) || native)
        && matches!(route, Route::Direct | Route::LumaView)
        && !matches!(req.hardware, Hardware::Off);
    // A native PiSP makes the deeper levels too, an extra back end pass each
    // (`NativeIspConfig::pyramid_all_levels`); another ISP's second output makes level 1.
    let isp_deepest = if native { pyramid.levels.min(3) } else { 1 };
    let isp_level = match pyramid.source {
        PyramidSource::Software | PyramidSource::Gpu => None,
        PyramidSource::PreferHardware => isp_possible.then_some(isp_deepest),
        PyramidSource::HardwareOnly => {
            if !isp_possible {
                return Err("hardware pyramid required but no ISP second output".into());
            }
            if pyramid.levels > isp_deepest {
                return Err(format!(
                    "hardware pyramid required for {} levels; the ISP provides {isp_deepest}",
                    pyramid.levels
                ));
            }
            Some(isp_deepest)
        }
    };
    for level in 1..=pyramid.levels {
        let (w, h) = (width >> level, height >> level);
        if isp_level.is_some_and(|deepest| level <= deepest) {
            let (detail, cost) = if level == 1 {
                (
                    format!("{w}x{h} from the ISP's second output"),
                    StepCost::ZERO,
                )
            } else {
                (
                    format!("{w}x{h} from an extra ISP pass"),
                    cost::pisp_pass(width, height),
                )
            };
            steps.push(PlanStep {
                kind: StepKind::Pyramid { level },
                execution: StepExecution::Hardware,
                detail,
                cost,
            });
        } else if pyramid.source == PyramidSource::Gpu && cfg!(feature = "gpu-wgpu") {
            let source_mp = megapixels(width >> (level - 1), height >> (level - 1));
            steps.push(PlanStep {
                kind: StepKind::Pyramid { level },
                execution: StepExecution::Gpu,
                detail: format!("{w}x{h} 2x2 box filter on the GPU (CPU where there is none)"),
                cost: StepCost::offloaded(
                    cost::GPU_LEVEL_MS_PER_MP * source_mp,
                    cost::GPU_COPY_MS_PER_MP * source_mp,
                ),
            });
        } else {
            let source_mp = megapixels(width >> (level - 1), height >> (level - 1));
            steps.push(PlanStep {
                kind: StepKind::Pyramid { level },
                execution: StepExecution::Cpu,
                detail: format!("{w}x{h} 2x2 box filter"),
                cost: StepCost::cpu(cost::BOX_LEVEL_MS_PER_MP * source_mp),
            });
        }
    }
    Ok(isp_level)
}

/// The ISP's pyramid levels of an ISP-cropped region (`region` in size) come from extra passes
/// of the region, not the second output.
pub(crate) fn from_region_passes(steps: &mut [PlanStep], region: (u32, u32)) {
    for step in steps {
        if let StepKind::Pyramid { level } = step.kind
            && step.execution == StepExecution::Hardware
        {
            step.detail = format!("level {level} from an extra ISP pass of the region");
            step.cost = cost::pisp_pass(region.0, region.1);
        }
    }
}
