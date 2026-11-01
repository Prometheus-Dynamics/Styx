//! [`Accelerators`] applied to a plan: what each stage's knob does to the request and to the
//! camera as the planner sees it.

use std::borrow::Cow;

use super::PlanRejection;
use super::native::native_isp;
use super::request::{Accelerators, FrameRequest, Hardware};
use crate::prelude::ProbedDevice;
use styx_core::prelude::PyramidSource;

/// `req` with its pyramid knob applied to its pyramid's source.
pub(super) fn request(req: &FrameRequest) -> FrameRequest {
    let mut req = req.clone();
    if let Some(pyramid) = &mut req.pyramid {
        pyramid.source = match (req.accelerators.pyramid, pyramid.source) {
            (_, PyramidSource::Gpu) | (Hardware::Auto, _) => pyramid.source,
            (Hardware::Off, _) => PyramidSource::Software,
            (_, _) => PyramidSource::HardwareOnly,
        };
    }
    req
}

/// `device` as the ISP knob leaves it: off, its native backends run the software ISP; required,
/// the backends that have only the software ISP are gone.
pub(super) fn device<'a>(
    device: &'a ProbedDevice,
    knobs: Accelerators,
    rejected: &mut Vec<PlanRejection>,
) -> Cow<'a, ProbedDevice> {
    match knobs.isp {
        Hardware::Off if device.backends.iter().any(|b| native_isp(b).is_some()) => {
            let mut device = device.clone();
            for backend in &mut device.backends {
                if native_isp(backend).is_some() {
                    for (key, value) in &mut backend.properties {
                        if key == "isp" {
                            *value = "software".into();
                        }
                    }
                }
            }
            Cow::Owned(device)
        }
        Hardware::Required
            if device
                .backends
                .iter()
                .any(|b| native_isp(b) == Some("software")) =>
        {
            let mut device = device.clone();
            device.backends.retain(|b| {
                let keep = native_isp(b) != Some("software");
                if !keep {
                    rejected.push(PlanRejection {
                        candidate: b.kind.to_string(),
                        reason: "a hardware ISP required, and this camera has none".into(),
                    });
                }
                keep
            });
            Cow::Owned(device)
        }
        _ => Cow::Borrowed(device),
    }
}

/// Whether the encoder knob allows a hardware (or a software) encoder.
pub(super) fn encoder_allowed(req: &FrameRequest, hardware: bool) -> bool {
    match req.accelerators.encoder {
        Hardware::Off => !hardware,
        Hardware::Required => hardware,
        _ => true,
    }
}

/// With the scaler knob required, the size the request wants must come from the ISP.
pub(super) fn check_scaler(
    req: &FrameRequest,
    isp_output: Option<(u32, u32)>,
    mode: (u32, u32),
) -> Result<(), String> {
    match req.size {
        Some((w, h))
            if req.accelerators.scaler == Hardware::Required
                && isp_output.is_none()
                && (w < mode.0 || h < mode.1) =>
        {
            Err(format!(
                "a hardware scaler required for {w}x{h}, and this route has none"
            ))
        }
        _ => Ok(()),
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::super::region_tests::pisp;
    use super::super::tests::registry;
    use super::super::{FrameRequest, Hardware, plan_frames_with};
    use super::*;
    use crate::planner::Frames;

    fn knobs(f: impl FnOnce(&mut Accelerators)) -> Accelerators {
        let mut a = Accelerators::default();
        f(&mut a);
        a
    }

    fn req(a: Accelerators) -> FrameRequest {
        Frames::nv12().accelerators(a)
    }

    #[test]
    fn isp_off_runs_the_software_isp_and_required_refuses_it() {
        let d = pisp("pisp");
        let off = knobs(|a| a.isp = Hardware::Off);
        let plan = plan_frames_with(&d, &req(off), &registry()).unwrap();
        assert_eq!(native_isp(&plan.device.backends[0]), Some("software"));
        assert_eq!(plan.start_config().accelerators.isp, Hardware::Off);

        let auto = plan_frames_with(&d, &req(Accelerators::default()), &registry()).unwrap();
        assert_eq!(native_isp(&auto.device.backends[0]), Some("pisp"));

        let required = knobs(|a| a.isp = Hardware::Required);
        assert!(plan_frames_with(&d, &req(required), &registry()).is_ok());
        let soft = pisp("software");
        assert!(plan_frames_with(&soft, &req(required), &registry()).is_err());
    }

    #[test]
    fn the_scaler_knob_decides_who_scales() {
        let d = pisp("pisp");
        let sized = |a| req(a).size(640, 400);
        let auto = plan_frames_with(&d, &sized(Accelerators::default()), &registry()).unwrap();
        assert!(auto.isp_output.is_some());
        let off = knobs(|a| a.scaler = Hardware::Off);
        let plan = plan_frames_with(&d, &sized(off), &registry()).unwrap();
        assert_eq!(plan.isp_output, None);
        let required = knobs(|a| a.scaler = Hardware::Required);
        assert!(plan_frames_with(&d, &sized(required), &registry()).is_ok());
        let usb = pisp("software");
        assert!(plan_frames_with(&usb, &sized(required), &registry()).is_err());
    }

    #[test]
    fn the_pyramid_knob_decides_where_levels_come_from() {
        let d = pisp("pisp");
        let levels = |a| req(a).pyramid(1);
        let auto = plan_frames_with(&d, &levels(Accelerators::default()), &registry()).unwrap();
        assert!(auto.isp_pyramid_level.is_some());
        let off = knobs(|a| a.pyramid = Hardware::Off);
        let plan = plan_frames_with(&d, &levels(off), &registry()).unwrap();
        assert_eq!(plan.isp_pyramid_level, None);
        let required = knobs(|a| a.pyramid = Hardware::Required);
        let soft = pisp("software");
        assert!(plan_frames_with(&soft, &levels(required), &registry()).is_err());
        assert!(plan_frames_with(&d, &levels(required), &registry()).is_ok());
    }

    #[test]
    fn the_encoder_knob_filters_encoders() {
        let r = |a| req(a);
        let off = r(knobs(|a| a.encoder = Hardware::Off));
        assert!(encoder_allowed(&off, false) && !encoder_allowed(&off, true));
        let required = r(knobs(|a| a.encoder = Hardware::Required));
        assert!(!encoder_allowed(&required, false) && encoder_allowed(&required, true));
        let auto = r(Accelerators::default());
        assert!(encoder_allowed(&auto, false) && encoder_allowed(&auto, true));
    }
}
