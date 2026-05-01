use super::*;
use crate::BackendKind;
use crate::capture_api::{CaptureRequest, make_virtual_rgb_device};
use crate::session::MediaPipelineBuilder;

struct FailingCodec {
    descriptor: CodecDescriptor,
}

impl FailingCodec {
    fn decoder() -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::RG24,
                output: FourCc::RG24,
                name: "failing",
                impl_name: "test",
            },
        }
    }
}

impl Codec for FailingCodec {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, _input: FrameLease) -> Result<FrameLease, CodecError> {
        Err(CodecError::Codec("forced failure".into()))
    }
}

#[test]
fn shared_output_estimate_uses_codec_output_format_for_4k_frames() {
    let resolution = Resolution::new(3840, 2160).expect("resolution");
    let input_format = MediaFormat::new(FourCc::MJPG, resolution, ColorSpace::Srgb);
    let pool = BufferPool::with_limits(1, 1024, 1);
    let mut payload = pool.lease();
    payload.resize(1024);
    let frame = FrameLease::single_plane(FrameMeta::new(input_format, 0), payload, 1024, 1024);
    let descriptor = CodecDescriptor {
        kind: CodecKind::Decoder,
        input: FourCc::MJPG,
        output: FourCc::RG24,
        name: "mjpeg",
        impl_name: "test",
    };

    assert_eq!(
        estimate_shared_output_bytes(&descriptor, &frame),
        Some(24_883_200)
    );
}

#[test]
fn fallible_next_preserves_decode_failure() {
    let device = make_virtual_rgb_device("fallible-decode", 2, 2, 30);
    let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
    let mut pipeline = MediaPipelineBuilder::new(request)
        .decoder(Arc::new(FailingCodec::decoder()))
        .without_encoder()
        .start()
        .expect("pipeline starts");

    let err = match pipeline.next_blocking_result(std::time::Duration::from_millis(250)) {
        Ok(_) => panic!("decode failure should be returned"),
        Err(err) => err,
    };
    assert_eq!(err.stage, PipelineStage::Decode);
    assert_eq!(err.component, "failing:test");
    assert!(err.message.contains("forced failure"));

    let report = pipeline.health_report();
    assert_eq!(report.recent_stage_errors, vec![err.clone()]);
    assert_eq!(pipeline.last_stage_error(), Some(err));
}

#[test]
fn pipeline_health_includes_capture_control_errors() {
    let device = make_virtual_rgb_device("capture-control-observability", 2, 2, 30);
    let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
    let pipeline = MediaPipelineBuilder::new(request)
        .without_decoder()
        .without_encoder()
        .start()
        .expect("pipeline starts");

    let err = pipeline
        .capture()
        .set_control(ControlId(42), ControlValue::Bool(true))
        .expect_err("virtual controls are unsupported");

    let report = pipeline.health_report();
    let capture_error = report
        .recent_stage_errors
        .iter()
        .find(|error| error.component == "virtual.control")
        .expect("capture control error is surfaced");

    assert_eq!(capture_error.stage, PipelineStage::Capture);
    assert_eq!(capture_error.message, err.to_string());
    assert_eq!(pipeline.last_stage_error(), Some(capture_error.clone()));
}
