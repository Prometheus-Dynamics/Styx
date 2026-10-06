//! Camera controls over a camera service's socket, against virtual cameras with controls:
//! accepted, clamped, refused (unsupported, read only, not permitted), read back, listed, kept
//! across a capture restart, a frame rate set by restarting the capture, and change events to
//! the other clients.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use styx::capture_api::make_virtual_device_with_controls;
use styx::ipc::{
    CameraService, ControlCaller, ControlPolicy, ControlRefusal, FrameClient, FrameServer,
    IpcError, SERVICE_FRAME_RATE, StandardControl,
};
use styx::prelude::*;

const SHARPNESS: ControlId = ControlId(0x100);
const TEMPERATURE: ControlId = ControlId(0x200);

fn control(
    id: u32,
    name: &str,
    kind: ControlKind,
    range: (ControlValue, ControlValue),
) -> ControlMeta {
    ControlMeta {
        id: ControlId(id),
        name: name.into(),
        kind,
        access: Access::ReadWrite,
        default: range.0.clone(),
        min: range.0,
        max: range.1,
        step: None,
        menu: None,
        metadata: ControlMetadata::default(),
    }
}

/// A 30 fps NV12 camera with exposure, gain, AE, sharpness and a read-only temperature.
fn camera(name: &str) -> ProbedDevice {
    let res = Resolution::new(320, 200).unwrap();
    let mut mode = Mode::with_interval(
        MediaFormat::new(FourCc::NV12, res, ColorSpace::Srgb),
        Interval::from_fps(30).unwrap(),
    );
    mode.intervals.push(Interval::from_fps(15).unwrap());
    let mut temperature = control(
        TEMPERATURE.0,
        "sensor_temperature",
        ControlKind::Int,
        (ControlValue::Int(40), ControlValue::Int(40)),
    );
    temperature.access = Access::ReadOnly;
    make_virtual_device_with_controls(
        name,
        [mode],
        vec![
            control(
                0xF400_0001,
                "exposure_time_us",
                ControlKind::Uint,
                (ControlValue::Uint(10), ControlValue::Uint(33_000)),
            ),
            control(
                0xF400_0002,
                "gain",
                ControlKind::Float,
                (ControlValue::Float(1.0), ControlValue::Float(16.0)),
            ),
            control(
                0xF400_0005,
                "ae_enable",
                ControlKind::Bool,
                (ControlValue::Bool(false), ControlValue::Bool(true)),
            ),
            control(
                SHARPNESS.0,
                "sharpness",
                ControlKind::Int,
                (ControlValue::Int(0), ControlValue::Int(10)),
            ),
            temperature,
        ],
    )
}

fn socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("styx-controls-{name}-{}.sock", std::process::id()))
}

fn frame(client: &FrameClient) -> FrameLease {
    for _ in 0..50 {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(200)) {
            return frame;
        }
    }
    panic!("no frame");
}

fn refusal(result: Result<impl std::fmt::Debug, IpcError>) -> ControlRefusal {
    match result {
        Err(IpcError::ControlRefused(refusal)) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn controls_are_applied_clamped_refused_and_announced() {
    let path = socket("apply");
    let _service = CameraService::new(camera("cam"))
        .keep_streaming()
        .serve(&path)
        .unwrap();
    let a = FrameClient::request(&path, &Frames::nv12()).unwrap();
    let b = FrameClient::request(&path, &Frames::nv12()).unwrap();
    let events = b.control_events().unwrap();
    drop(frame(&a));

    // Accepted, in effect, and announced with who changed it.
    let applied = a.set_exposure_us(1000).unwrap();
    assert_eq!(applied.value, ControlValue::Uint(1000));
    assert!(!applied.clamped && !applied.deferred && !applied.restarted);
    let RecvOutcome::Data(event) = events.recv(Duration::from_secs(2)) else {
        panic!("no event");
    };
    assert_eq!(event.standard, Some(StandardControl::ExposureUs));
    assert_eq!(event.value, ControlValue::Uint(1000));
    assert_eq!(event.by, a.client_id());
    assert_ne!(a.client_id(), b.client_id());

    // Out of range: clamped, and the clamped value is in effect.
    let clamped = a.set_exposure_us(100_000).unwrap();
    assert!(clamped.clamped);
    assert_eq!(clamped.value, ControlValue::Uint(33_000));
    assert_eq!(
        b.get_control(StandardControl::ExposureUs).unwrap(),
        ControlValue::Uint(33_000)
    );
    // Any numeric type is taken for the control's type.
    let gain = b.set_gain(2.5).unwrap();
    assert_eq!(gain.value, ControlValue::Float(2.5));
    let sharp = b.set_control(SHARPNESS, ControlValue::Float(7.4)).unwrap();
    assert_eq!(sharp.value, ControlValue::Int(7));
    assert!(sharp.clamped);
    assert!(b.set_ae(false).is_ok());

    // Refused: read only, unsupported, wrong type.
    assert!(matches!(
        refusal(a.set_control(TEMPERATURE, ControlValue::Int(1))),
        ControlRefusal::ReadOnly(_)
    ));
    assert!(matches!(
        refusal(a.set_colour_temperature(5000)),
        ControlRefusal::Unsupported(_)
    ));
    assert!(matches!(
        refusal(a.set_control(ControlId(0x999), ControlValue::Int(1))),
        ControlRefusal::Unsupported(_)
    ));
    assert!(matches!(
        refusal(a.set_control(
            SHARPNESS,
            ControlValue::Rect(ControlRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            })
        )),
        ControlRefusal::Invalid(_)
    ));

    // The list: every control, its value now, the standard control it answers, and the frame
    // rate the service sets by restarting.
    let list = a.controls().unwrap();
    let find = |id: ControlId| list.iter().find(|d| d.meta.id == id).unwrap();
    let exposure = find(ControlId(0xF400_0001));
    assert_eq!(exposure.standard, Some(StandardControl::ExposureUs));
    assert_eq!(exposure.current, Some(ControlValue::Uint(33_000)));
    assert!(exposure.writable);
    assert!(!find(TEMPERATURE).writable);
    assert_eq!(find(TEMPERATURE).current, Some(ControlValue::Int(40)));
    assert_eq!(find(SHARPNESS).current, Some(ControlValue::Int(7)));
    assert!(find(SERVICE_FRAME_RATE).writable);

    // Events for every accepted change since (b's own too), in order.
    let mut seen = Vec::new();
    while let RecvOutcome::Data(event) = events.recv(Duration::from_millis(300)) {
        seen.push((event.id, event.by));
    }
    assert_eq!(
        seen,
        vec![
            (ControlId(0xF400_0001), a.client_id()),
            (ControlId(0xF400_0002), b.client_id()),
            (SHARPNESS, b.client_id()),
            (ControlId(0xF400_0005), b.client_id()),
        ]
    );
}

#[test]
fn a_frame_rate_restarts_the_capture_and_keeps_the_controls() {
    let path = socket("fps");
    let service = CameraService::new(camera("cam"))
        .keep_streaming()
        .serve(&path)
        .unwrap();
    let a = FrameClient::request(&path, &Frames::nv12()).unwrap();
    let b = FrameClient::request(&path, &Frames::nv12()).unwrap();
    drop((frame(&a), frame(&b)));
    a.set_control(SHARPNESS, ControlValue::Int(3)).unwrap();
    let restarts = service.stats().restarts;

    let applied = a.set_fps(15.0).unwrap();
    assert!(applied.restarted, "{applied:?}");
    assert_eq!(applied.id, SERVICE_FRAME_RATE);
    assert_eq!(applied.value, ControlValue::Float(15.0));
    assert_eq!(service.stats().restarts, restarts + 1);
    assert_eq!(
        a.get_control(StandardControl::FrameRate).unwrap(),
        ControlValue::Float(15.0)
    );
    // Both clients keep their frames, and the control set before is in effect again.
    drop((frame(&a), frame(&b)));
    assert_eq!(
        b.get_control(SHARPNESS).unwrap(),
        ControlValue::Int(3),
        "lost across the restart"
    );

    // A policy that does not restart refuses it with the reason.
    let path = socket("fps-no-restart");
    let _service = CameraService::new(camera("cam"))
        .keep_streaming()
        .control_policy(ControlPolicy::anyone().no_restart())
        .serve(&path)
        .unwrap();
    let c = FrameClient::request(&path, &Frames::nv12()).unwrap();
    match refusal(c.set_fps(15.0)) {
        ControlRefusal::NotPermitted(why) => assert!(why.contains("restart"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_policy_decides_who_may_change_what() {
    let path = socket("owner");
    let _service = CameraService::new(camera("cam"))
        .keep_streaming()
        .control_policy(ControlPolicy::owner_only().read_only_control(StandardControl::Gain))
        .serve(&path)
        .unwrap();
    let owner = FrameClient::request(&path, &Frames::nv12()).unwrap();
    let other = FrameClient::request(&path, &Frames::nv12()).unwrap();
    assert!(owner.set_exposure_us(500).is_ok());
    assert!(matches!(
        refusal(other.set_exposure_us(600)),
        ControlRefusal::NotPermitted(_)
    ));
    // Reading is always allowed.
    assert_eq!(
        other.get_control(StandardControl::ExposureUs).unwrap(),
        ControlValue::Uint(500)
    );
    // Read only by policy, for the owner too.
    assert!(matches!(
        refusal(owner.set_gain(2.0)),
        ControlRefusal::NotPermitted(_)
    ));
    let listed = other.controls().unwrap();
    assert!(listed.iter().all(|d| !d.writable));
    // The owner leaves: the next client becomes the owner.
    drop(owner);
    std::thread::sleep(Duration::from_millis(300));
    assert!(other.set_exposure_us(700).is_ok());

    // An allow-list, given the caller.
    let path = socket("allow");
    let _service = CameraService::new(camera("cam"))
        .keep_streaming()
        .control_policy(
            ControlPolicy::anyone()
                .allow(|caller: &ControlCaller, id| caller.client.is_some() && id != SHARPNESS),
        )
        .serve(&path)
        .unwrap();
    let client = FrameClient::request(&path, &Frames::nv12()).unwrap();
    assert!(client.set_exposure_us(800).is_ok());
    assert!(matches!(
        refusal(client.set_control(SHARPNESS, ControlValue::Int(1))),
        ControlRefusal::NotPermitted(_)
    ));

    // Nobody: read only.
    let path = socket("read-only");
    let _service = CameraService::new(camera("cam"))
        .keep_streaming()
        .control_policy(ControlPolicy::read_only())
        .serve(&path)
        .unwrap();
    let client = FrameClient::request(&path, &Frames::nv12()).unwrap();
    assert!(matches!(
        refusal(client.set_exposure_us(800)),
        ControlRefusal::NotPermitted(_)
    ));
}

#[test]
fn controls_need_a_camera_service() {
    let path = socket("frame-server");
    let _server = FrameServer::bind(&path).unwrap();
    let client = FrameClient::connect(&path).unwrap();
    assert!(matches!(
        client.set_exposure_us(1000),
        Err(IpcError::Rejected(_))
    ));
}
