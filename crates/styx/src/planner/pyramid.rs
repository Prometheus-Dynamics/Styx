//! Pyramid levels: from the ISP's second output (or an extra pass of an ISP-cropped region),
//! else 2x2 box filters on the CPU.

use styx_core::prelude::*;

use super::cost::{self, StepCost, megapixels};
use super::request::{FrameRequest, Hardware};
use super::routes::{Route, has_isp_second_output, native_isp_outputs};
use super::{PlanStep, StepExecution, StepKind};
use crate::prelude::{Mode, ProbedBackend};

/// Pyramid steps; returns the level the ISP produces, if any.
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
    let isp_level = match pyramid.source {
        PyramidSource::Software => None,
        PyramidSource::PreferHardware => isp_possible.then_some(1),
        PyramidSource::HardwareOnly => {
            if !isp_possible {
                return Err("hardware pyramid required but no ISP second output".into());
            }
            if pyramid.levels > 1 {
                return Err(format!(
                    "hardware pyramid required for {} levels; the ISP provides one",
                    pyramid.levels
                ));
            }
            Some(1)
        }
    };
    for level in 1..=pyramid.levels {
        let (w, h) = (width >> level, height >> level);
        if isp_level == Some(level) {
            steps.push(PlanStep {
                kind: StepKind::Pyramid { level },
                execution: StepExecution::Hardware,
                detail: format!("{w}x{h} from the ISP's second output"),
                cost: StepCost::ZERO,
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
