use super::*;
use crate::BackendKind;
use crate::capture_api::{CaptureRequest, make_virtual_rgb_device};
use crate::service::{
    PipelineWorkerEvent, PipelineWorkerStopReason, ServiceEventCursor, StyxServiceEvent,
    StyxServiceRuntime,
};
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

#[cfg(target_os = "linux")]
#[test]
fn memory_stats_report_shared_codec_pool_capacity() {
    let device = make_virtual_rgb_device("shared-pool-memory-stats", 2, 2, 30);
    let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
    let mut pipeline = MediaPipelineBuilder::new(request)
        .without_decoder()
        .without_encoder()
        .start()
        .expect("pipeline starts");
    let small_resolution = Resolution::new(320, 240).expect("small resolution");
    let large_resolution = Resolution::new(3840, 2160).expect("large resolution");
    let frame_pool = BufferPool::with_limits(1, 1024, 1);
    let mut small_payload = frame_pool.lease();
    small_payload.resize(1024);
    let mut large_payload = frame_pool.lease();
    large_payload.resize(1024);
    let small_frame = FrameLease::single_plane(
        FrameMeta::new(
            MediaFormat::new(FourCc::MJPG, small_resolution, ColorSpace::Srgb),
            0,
        ),
        small_payload,
        1024,
        1024,
    );
    let large_frame = FrameLease::single_plane(
        FrameMeta::new(
            MediaFormat::new(FourCc::MJPG, large_resolution, ColorSpace::Srgb),
            1,
        ),
        large_payload,
        1024,
        1024,
    );
    let decoder = CodecDescriptor {
        kind: CodecKind::Decoder,
        input: FourCc::MJPG,
        output: FourCc::RG24,
        name: "mjpeg",
        impl_name: "test",
    };
    let encoder = CodecDescriptor {
        kind: CodecKind::Encoder,
        input: FourCc::RG24,
        output: FourCc::MJPG,
        name: "mjpeg",
        impl_name: "test",
    };

    pipeline
        .shared_decode_pool_for(&decoder, &small_frame)
        .expect("small decode pool");
    pipeline
        .shared_decode_pool_for(&decoder, &large_frame)
        .expect("large decode pool");
    pipeline
        .shared_encode_pool_for(&encoder, &large_frame)
        .expect("encode pool");

    let memory = pipeline.memory_stats();
    let decode_pool = memory.shared_decode_pool.expect("decode pool stats");
    let encode_pool = memory.shared_encode_pool.expect("encode pool stats");

    assert_eq!(decode_pool.chunk_size, 24_883_200);
    assert_eq!(decode_pool.free, 2);
    assert_eq!(decode_pool.max_free, 4);
    assert_eq!(decode_pool.free_bytes, 49_766_400);
    assert_eq!(encode_pool.chunk_size, 64 * 1024);
    assert_eq!(encode_pool.max_free, 4);
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
fn blocking_worker_returns_decode_failure() {
    let device = make_virtual_rgb_device("worker-fallible-decode", 2, 2, 30);
    let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
    let pipeline = MediaPipelineBuilder::new(request)
        .decoder(Arc::new(FailingCodec::decoder()))
        .without_encoder()
        .start()
        .expect("pipeline starts");

    let err = pipeline
        .spawn_worker()
        .join()
        .expect("worker thread")
        .expect_err("decode failure should be returned by worker");

    assert_eq!(err.stage, PipelineStage::Decode);
    assert_eq!(err.component, "failing:test");
    assert!(err.message.contains("forced failure"));
}

#[test]
fn blocking_worker_emits_terminal_failure_service_event() {
    let service = Arc::new(std::sync::Mutex::new(StyxServiceRuntime::new()));
    let mut cursor = ServiceEventCursor::from_start();
    let device = make_virtual_rgb_device("worker-failure-event", 2, 2, 30);
    let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
    let pipeline = MediaPipelineBuilder::new(request)
        .decoder(Arc::new(FailingCodec::decoder()))
        .without_encoder()
        .service_runtime(Arc::clone(&service))
        .start()
        .expect("pipeline starts");

    let err = pipeline
        .spawn_worker()
        .join()
        .expect("worker thread")
        .expect_err("decode failure should be returned by worker");

    let service = service.lock().expect("service lock");
    let poll = service.poll_events(&mut cursor);
    let event = poll
        .events()
        .iter()
        .find_map(|event| match &event.event {
            StyxServiceEvent::Pipeline(PipelineWorkerEvent::Stopped { reason }) => Some(reason),
            _ => None,
        })
        .expect("pipeline worker terminal event");

    assert_eq!(event, &PipelineWorkerStopReason::StageFailed(err));
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
