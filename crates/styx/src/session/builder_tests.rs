use super::*;
use crate::{BackendHandle, BackendKind, DeviceIdentity, ProbedBackend, ProbedDevice};
use std::num::NonZeroU32;
use styx_capture::prelude::{
    CaptureDescriptor, ColorSpace, FourCc, Interval, MediaFormat, Mode, ModeId, RecvOutcome,
    Resolution,
};

fn virtual_device() -> ProbedDevice {
    let interval = Interval {
        numerator: NonZeroU32::new(1).unwrap(),
        denominator: NonZeroU32::new(30).unwrap(),
    };
    let format = MediaFormat::new(
        FourCc::RG24,
        Resolution::new(2, 2).unwrap(),
        ColorSpace::Srgb,
    );
    let mode = Mode {
        id: ModeId {
            format,
            interval: Some(interval),
        },
        format,
        intervals: smallvec::smallvec![interval],
        interval_stepwise: None,
    };
    ProbedDevice {
        identity: DeviceIdentity {
            display: "virtual-test".into(),
            keys: vec!["virtual-test".into()],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::Virtual,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes: vec![mode],
                controls: vec![],
            },
            properties: vec![],
        }],
    }
}

#[test]
fn builder_start_runs_linear_facade_through_graph_pipeline() {
    let device = virtual_device();
    let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
    let mut pipeline = MediaPipelineBuilder::new(request)
        .decoder(Arc::new(PassthroughDecoder::new(FourCc::RG24)))
        .graph_execution()
        .shared_decode_output(false)
        .without_encoder()
        .start()
        .expect("start graph-backed pipeline");

    match pipeline.next_blocking(std::time::Duration::from_millis(250)) {
        RecvOutcome::Data(frame) => {
            assert_eq!(frame.meta().format.code, FourCc::RG24);
        }
        RecvOutcome::Empty => panic!("expected frame from graph-backed pipeline, got empty"),
        RecvOutcome::Closed => panic!("expected frame from graph-backed pipeline, got closed"),
    }
    let control_result = pipeline
        .submit_control_event(crate::graph::StyxControlEvent::Get {
            id: styx_core::prelude::ControlId(1),
        })
        .expect("control event should route alongside frame nodes");
    assert!(!control_result.is_ok());
    assert!(pipeline.graph_telemetry().is_some());
}

#[test]
fn builder_can_force_linear_execution_with_graph_feature_enabled() {
    let device = virtual_device();
    let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
    let mut pipeline = MediaPipelineBuilder::new(request)
        .decoder(Arc::new(PassthroughDecoder::new(FourCc::RG24)))
        .linear_execution()
        .shared_decode_output(false)
        .without_encoder()
        .start()
        .expect("start linear pipeline");

    match pipeline.next_blocking(std::time::Duration::from_millis(250)) {
        RecvOutcome::Data(frame) => {
            assert_eq!(frame.meta().format.code, FourCc::RG24);
        }
        RecvOutcome::Empty => panic!("expected frame from linear pipeline, got empty"),
        RecvOutcome::Closed => panic!("expected frame from linear pipeline, got closed"),
    }
    assert!(pipeline.graph_telemetry().is_none());
    assert!(
        pipeline
            .submit_control_event(crate::graph::StyxControlEvent::Get {
                id: styx_core::prelude::ControlId(1),
            })
            .is_err()
    );
}

#[test]
fn graph_backed_pipeline_routes_control_events_through_graph() {
    let device = virtual_device();
    let request = CaptureRequest::new(&device).backend(BackendKind::Virtual);
    let service = Arc::new(std::sync::Mutex::new(
        crate::service::StyxServiceRuntime::new(),
    ));
    let mut cursor = service.lock().expect("service lock").subscribe_from_start();
    let mut pipeline = MediaPipelineBuilder::new(request)
        .raw_frames()
        .service_runtime(Arc::clone(&service))
        .start()
        .expect("start graph-backed raw pipeline");

    let result = pipeline
        .submit_control_event(crate::graph::StyxControlEvent::Set {
            id: styx_core::prelude::ControlId(1),
            value: styx_core::prelude::ControlValue::Bool(true),
        })
        .expect("control event routed through graph");

    assert!(!result.is_ok());
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|err| err.contains("control plane not available"))
    );
    let _ = pipeline.health_report();
    let events = {
        let service = service.lock().expect("service lock");
        service.poll_events(&mut cursor).events().to_vec()
    };
    assert!(events.iter().any(|event| {
        matches!(
            event.event,
            crate::service::StyxServiceEvent::Control(crate::graph::StyxControlResult { .. })
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| { matches!(event.event, crate::service::StyxServiceEvent::Health(_)) })
    );
    assert!(pipeline.graph_telemetry().is_some());
}
