//! Who brings frames to the size a request asks for: the ISP (it outputs that size) or, where
//! the route delivers them larger, the chip's 2D scaler (RGA, or a V4L2 M2M scaler such as the
//! K230's). The 2D scaler only runs on chips [`styx_pipeline::chips`] lists with one, so other
//! machines plan as they did.
//!
//! The 2D scaler stage is written, untested on board.

use std::sync::Arc;

use styx_codec::Codec;
use styx_core::prelude::*;

use super::cost::{self, megapixels};
use super::native::{native_isp, raw_bayer};
use super::request::{FrameRequest, Hardware};
use super::routes::{Route, has_isp_second_output};
use super::{PlanStep, StepExecution, StepKind};
use crate::BackendKind;
use crate::prelude::{Mode, ProbedBackend};

/// The smallest even size with `from`'s aspect ratio (so its field of view) that covers
/// `target`; `None` when that is `from` itself.
fn covering_size(target: (u32, u32), from: (u32, u32)) -> Option<(u32, u32)> {
    let scale =
        (f64::from(target.0) / f64::from(from.0)).max(f64::from(target.1) / f64::from(from.1));
    let even = |v: f64| ((v.ceil() as u32).next_multiple_of(2)).max(2);
    let (w, h) = (
        even(f64::from(from.0) * scale),
        even(f64::from(from.1) * scale),
    );
    (w < from.0 || h < from.1).then_some((w.min(from.0), h.min(from.1)))
}

/// Size the ISP should deliver for `req.size`: the smallest even size with the
/// mode's aspect ratio (so its field of view) that covers it. Only for cameras behind a
/// scaling ISP, on routes that pass frames through without decoding.
pub(super) fn isp_output(
    backend: &ProbedBackend,
    route: &Route,
    req: &FrameRequest,
    code: FourCc,
    mode: (u32, u32),
) -> Option<(u32, u32)> {
    let (tw, th) = req.size?;
    // Routes that pass frames through, or decode uncompressed ones (any size the ISP makes).
    let scalable = match route {
        Route::Direct | Route::LumaView => true,
        Route::Decode { .. } => !code.is_compressed(),
        // The encoder takes any size the ISP makes.
        Route::Encode { decoder, .. } => decoder.is_none() && !code.is_compressed(),
    };
    let scaling_isp = has_isp_second_output(backend)
        || (backend.kind == BackendKind::Native
            && native_isp(backend) == Some("pisp")
            && !raw_bayer(code));
    if !scaling_isp
        || !scalable
        || matches!(req.hardware, Hardware::Off)
        || matches!(req.accelerators.scaler, Hardware::Off)
        || (tw >= mode.0 && th >= mode.1)
    {
        return None;
    }
    covering_size((tw, th), mode)
}

/// The 2D scaler a plan uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum M2mEngine {
    #[cfg(all(feature = "codec-rga", target_os = "linux"))]
    Rga,
    #[cfg(all(feature = "codec-v4l2m2m", target_os = "linux"))]
    V4l2,
    #[cfg(test)]
    Test,
}

/// A plan's 2D scaling step: frames of `format` scaled to `size`, in that format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct M2mScale {
    pub size: (u32, u32),
    pub format: FourCc,
    pub engine: M2mEngine,
}

impl M2mScale {
    /// The plan's step for frames of `from` in size.
    pub(super) fn step(self, from: (u32, u32)) -> PlanStep {
        let (w, h) = self.size;
        PlanStep {
            kind: StepKind::Scale,
            execution: StepExecution::Hardware,
            detail: format!(
                "{} {}x{} -> {w}x{h} on the {:?} 2D scaler",
                self.format, from.0, from.1, self.engine
            ),
            cost: cost::m2m_scale(megapixels(from.0, from.1)),
        }
    }

    /// A scaler of its own for one group of consumers (it holds that stream's buffers).
    #[allow(unused_variables)]
    pub(super) fn converter(self) -> Option<Arc<dyn Codec>> {
        let (format, size) = (self.format, Some(self.size));
        match self.engine {
            #[cfg(all(feature = "codec-rga", target_os = "linux"))]
            M2mEngine::Rga => {
                use styx_codec::rga::{RgaConverter, RgaOptions};
                Some(Arc::new(RgaConverter::new(
                    format,
                    format,
                    size,
                    RgaOptions::default(),
                )))
            }
            #[cfg(all(feature = "codec-v4l2m2m", target_os = "linux"))]
            M2mEngine::V4l2 => {
                use styx_codec::v4l2m2m::{V4l2M2mConverter, V4l2M2mOptions};
                Some(Arc::new(V4l2M2mConverter::new(
                    format,
                    format,
                    size,
                    V4l2M2mOptions::default(),
                )))
            }
            #[cfg(test)]
            M2mEngine::Test => Some(Arc::new(test_engine::Resizer::new(format, size))),
        }
    }
}

/// The 2D scaling a route's frames need to reach the request's size, if the chip has a scaler
/// for their format. `from` is the size the route delivers before any scaling.
pub(super) fn m2m_scale(
    route: &Route,
    req: &FrameRequest,
    mode: &Mode,
    from: (u32, u32),
    decode_scale: u8,
    isp_output: Option<(u32, u32)>,
) -> Option<M2mScale> {
    if isp_output.is_some()
        || decode_scale != 1
        || matches!(req.hardware, Hardware::Off)
        || matches!(req.accelerators.scaler, Hardware::Off)
    {
        return None;
    }
    let size = covering_size(req.size?, from)?;
    let format = match route {
        Route::Direct | Route::LumaView => mode.format.code,
        // This decoder crops and aligns as it decodes: it leaves no frame to scale.
        Route::Decode { decoder, .. } if decoder.descriptor().impl_name == "turbojpeg-luma" => {
            return None;
        }
        Route::Decode { decoder, .. } => decoder.descriptor().output,
        Route::Encode {
            decoder: Some(d), ..
        } => d.descriptor().output,
        Route::Encode { decoder: None, .. } => mode.format.code,
    };
    if format.is_compressed() || raw_bayer(format) {
        return None;
    }
    Some(M2mScale {
        size,
        format,
        engine: engine(format)?,
    })
}

/// The chip's 2D scaler, when it can scale `format`.
#[allow(unused_variables)]
fn engine(format: FourCc) -> Option<M2mEngine> {
    #[cfg(test)]
    if test_engine::enabled() {
        return Some(M2mEngine::Test);
    }
    #[cfg(all(feature = "codec-rga", target_os = "linux"))]
    if styx_codec::rga::RgaConverter::available(format, format) {
        // The node only exists on a Rockchip chip.
        return Some(M2mEngine::Rga);
    }
    #[cfg(all(feature = "codec-v4l2m2m", feature = "native", target_os = "linux"))]
    if chip_has_v4l2_scaler() && styx_codec::v4l2m2m::V4l2M2mConverter::available(format, format) {
        return Some(M2mEngine::V4l2);
    }
    None
}

/// Whether this chip's 2D scaler is a V4L2 M2M one. Read once: planning asks per candidate.
/// Other machines' M2M nodes (a Raspberry Pi's codec and ISP ones) are not for this stage.
#[cfg(all(feature = "codec-v4l2m2m", feature = "native", target_os = "linux"))]
fn chip_has_v4l2_scaler() -> bool {
    use styx_pipeline::chips::{self, ScalerSupport};
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| chips::detect().is_some_and(|chip| chip.scaler == ScalerSupport::V4l2M2m))
}

#[cfg(test)]
pub(super) mod test_engine {
    //! A stand-in 2D scaler: planning and the preparer's wiring, without a Rockchip or K230.

    use std::cell::Cell;

    use styx_codec::{Codec, CodecDescriptor, CodecError, CodecKind};
    use styx_core::prelude::*;

    thread_local!(static ENABLED: Cell<bool> = const { Cell::new(false) });

    /// Plans made on this thread while the guard lives see a 2D scaler.
    pub(in super::super) struct Enabled;

    impl Enabled {
        pub(in super::super) fn new() -> Self {
            ENABLED.set(true);
            Self
        }
    }

    impl Drop for Enabled {
        fn drop(&mut self) {
            ENABLED.set(false);
        }
    }

    pub(super) fn enabled() -> bool {
        ENABLED.get()
    }

    /// Hands the frame on with the scaled size in its metadata.
    pub(super) struct Resizer {
        descriptor: CodecDescriptor,
        size: Option<(u32, u32)>,
    }

    impl Resizer {
        pub(super) fn new(format: FourCc, size: Option<(u32, u32)>) -> Self {
            let descriptor = CodecDescriptor {
                kind: CodecKind::Converter,
                input: format,
                output: format,
                name: "test-scale",
                impl_name: "test",
            };
            Self { descriptor, size }
        }
    }

    impl Codec for Resizer {
        fn descriptor(&self) -> &CodecDescriptor {
            &self.descriptor
        }

        fn process(&self, mut input: FrameLease) -> Result<FrameLease, CodecError> {
            let (w, h) = self.size.expect("a target size");
            input.meta_mut().format.resolution = Resolution::new(w, h).expect("a size");
            Ok(input)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{device, mode, registry};
    use super::super::{Frames, plan_frames_with};
    use super::test_engine::Enabled;
    use super::*;
    use crate::BackendHandle;
    use crate::planner::Hardware;
    use crate::prelude::ProbedDevice;

    fn camera() -> ProbedDevice {
        device(
            BackendKind::V4l2,
            BackendHandle::Virtual,
            vec![mode(FourCc::NV12, 1920, 1080, 30)],
        )
    }

    #[test]
    fn a_chip_with_a_scaler_delivers_the_size_asked() {
        let _scaler = Enabled::new();
        let plan =
            plan_frames_with(&camera(), &Frames::nv12().size(640, 360), &registry()).unwrap();
        assert_eq!(plan.output_resolution(), (640, 360));
        assert!(plan.unmet.is_empty(), "{:?}", plan.unmet);
        let scale = plan
            .steps
            .iter()
            .find(|s| s.kind == StepKind::Scale)
            .unwrap();
        assert_eq!(scale.execution, StepExecution::Hardware);
        assert_eq!(plan.isp_output, None);
    }

    #[test]
    fn no_scaler_leaves_the_frames_as_the_route_delivers_them() {
        let req = Frames::nv12().size(640, 360);
        let plan = plan_frames_with(&camera(), &req, &registry()).unwrap();
        assert_eq!(plan.output_resolution(), (1920, 1080));
        assert_eq!(plan.m2m_scale, None);
    }

    #[test]
    fn the_scaler_knob_turns_it_off_or_requires_it() {
        let _scaler = Enabled::new();
        let mut req = Frames::nv12().size(640, 360);
        req.accelerators.scaler = Hardware::Off;
        let plan = plan_frames_with(&camera(), &req, &registry()).unwrap();
        assert_eq!(plan.m2m_scale, None);
        req.accelerators.scaler = Hardware::Required;
        let plan = plan_frames_with(&camera(), &req, &registry()).unwrap();
        assert!(plan.m2m_scale.is_some());
    }

    #[test]
    fn frames_already_small_enough_are_not_scaled() {
        let _scaler = Enabled::new();
        let req = Frames::nv12().size(1920, 1080);
        let plan = plan_frames_with(&camera(), &req, &registry()).unwrap();
        assert_eq!(plan.m2m_scale, None);
    }

    #[test]
    fn prepared_frames_reach_the_size_asked() {
        use super::super::RoiHandle;
        use super::super::start::FramePreparer;

        let _scaler = Enabled::new();
        let plan =
            plan_frames_with(&camera(), &Frames::nv12().size(640, 360), &registry()).unwrap();
        let preparer = FramePreparer::new(&plan, RoiHandle::default());
        let format = plan.mode.format;
        let (w, h) = (1920usize, 1080usize);
        let pool = BufferPool::with_limits(1, w * h * 3 / 2, 2);
        let meta = FrameMeta::new(format, 1);
        let plane = |len: usize| PlaneLayout {
            offset: 0,
            len,
            stride: w,
        };
        let frame = FrameLease::multi_plane(
            meta,
            vec![pool.lease(), pool.lease()].into(),
            vec![plane(w * h), plane(w * h / 2)].into(),
        );
        let out = preparer.process(frame).unwrap();
        let res = out.meta().format.resolution;
        assert_eq!((res.width.get(), res.height.get()), (640, 360));
    }
}
