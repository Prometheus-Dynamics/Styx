//! Control clients (no frames) and clients that connect without blocking, against virtual
//! cameras: a control client sets, reads, lists and follows controls while it takes no frames,
//! never joins the frame plan, restarts or starts a capture, and the policy applies to it; a
//! frame or control client made without blocking returns at once when the service is not
//! there, reports "not ready" until it is, and starts delivering once it appears; readiness
//! through `poll(2)` and awaited on styx-graph's `block_on`.

#![cfg(target_os = "linux")]

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_device_with_controls;
use styx::ipc::{
    CameraService, CameraServiceHandle, ControlClient, ControlPolicy, ControlRefusal, FrameClient,
    IpcError, StandardControl,
};
use styx::prelude::*;
use styx_graph::rt::block_on;

const EXPOSURE: ControlId = ControlId(0xF400_0001);

fn control(
    id: u32,
    name: &str,
    kind: ControlKind,
    min: ControlValue,
    max: ControlValue,
) -> ControlMeta {
    ControlMeta {
        id: ControlId(id),
        name: name.into(),
        kind,
        access: Access::ReadWrite,
        default: min.clone(),
        min,
        max,
        step: None,
        menu: None,
        metadata: ControlMetadata::default(),
    }
}

/// A 60 fps NV12 camera with exposure and gain.
fn camera(name: &str) -> ProbedDevice {
    let mode = Mode::with_interval(
        MediaFormat::new(
            FourCc::NV12,
            Resolution::new(160, 100).unwrap(),
            ColorSpace::Srgb,
        ),
        Interval::from_fps(60).unwrap(),
    );
    make_virtual_device_with_controls(
        name,
        [mode],
        vec![
            control(
                EXPOSURE.0,
                "exposure_time_us",
                ControlKind::Uint,
                ControlValue::Uint(10),
                ControlValue::Uint(33_000),
            ),
            control(
                0xF400_0002,
                "gain",
                ControlKind::Float,
                ControlValue::Float(1.0),
                ControlValue::Float(16.0),
            ),
        ],
    )
}

fn socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("styx-ctlc-{name}-{}.sock", std::process::id()))
}

fn serve(path: &PathBuf) -> CameraServiceHandle {
    CameraService::with_cameras(vec![camera("left"), camera("right")])
        .keep_streaming()
        .serve(path)
        .unwrap()
}

fn frame(client: &FrameClient) -> FrameLease {
    for _ in 0..50 {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(200)) {
            return frame;
        }
    }
    panic!("no frame");
}

/// Wait up to `ms` for `fd` to be readable.
fn readable(fd: i32, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: a valid pollfd for the call's duration.
    unsafe { libc::poll(&mut p, 1, ms) };
    p.revents != 0
}

fn refusal(result: Result<impl std::fmt::Debug, IpcError>) -> ControlRefusal {
    match result {
        Err(IpcError::ControlRefused(refusal)) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_control_client_sets_reads_and_follows_controls_without_frames() {
    let path = socket("follow");
    let service = serve(&path);
    let frames = FrameClient::request_camera(&path, "left", &Frames::nv12()).unwrap();
    drop(frame(&frames));
    let before = service.stats();
    let plan = service.plan();

    let controls = ControlClient::connect_camera(&path, "left").unwrap();
    assert!(controls.is_connected());
    // Set, read back, listed, by this client and announced to it (by nobody's frames).
    let applied = controls.set_exposure_us(1000).unwrap();
    assert_eq!(applied.value, ControlValue::Uint(1000));
    assert!(!applied.deferred && !applied.restarted, "{applied:?}");
    assert_eq!(
        controls.get_control(StandardControl::ExposureUs).unwrap(),
        ControlValue::Uint(1000)
    );
    let list = controls.controls().unwrap();
    let exposure = list.iter().find(|d| d.meta.id == EXPOSURE).unwrap();
    assert_eq!(exposure.current, Some(ControlValue::Uint(1000)));
    assert!(exposure.writable);
    let RecvOutcome::Data(event) = controls.recv_event(Duration::from_secs(2)) else {
        panic!("no event");
    };
    assert_eq!((event.id, event.by), (EXPOSURE, None));

    // A frame client's change reaches it: the descriptor becomes readable.
    frames.set_gain(2.0).unwrap();
    assert!(readable(controls.as_raw_fd(), 2000));
    let RecvOutcome::Data(event) = controls.try_event() else {
        panic!("no event after readable");
    };
    assert_eq!(event.standard, Some(StandardControl::Gain));
    assert_eq!(event.by, frames.client_id());

    // The same, awaited on any executor.
    let applied =
        block_on(controls.set_control_async(StandardControl::Gain, ControlValue::Float(3.0)))
            .unwrap();
    assert_eq!(applied.value, ControlValue::Float(3.0));
    assert_eq!(
        block_on(controls.get_control_async(EXPOSURE)).unwrap(),
        ControlValue::Uint(1000)
    );
    assert_eq!(
        block_on(controls.controls_async()).unwrap().len(),
        list.len()
    );
    let RecvOutcome::Data(event) = block_on(controls.next_event()) else {
        panic!("no event awaited");
    };
    assert_eq!(event.value, ControlValue::Float(3.0));

    // Never a frame client: no plan change, no restart, no frames, no buffers.
    let during = service.stats();
    assert_eq!(during.clients, 1);
    assert_eq!(during.restarts, before.restarts);
    assert_eq!(service.plan(), plan);
    assert_eq!(service.metrics().client_metrics.len(), 1);
    drop(controls);
    std::thread::sleep(Duration::from_millis(300));
    let after = service.stats();
    assert_eq!((after.clients, after.restarts), (1, before.restarts));
    assert_eq!(service.plan(), plan);
    drop(frame(&frames));
}

#[test]
fn a_control_client_alone_starts_no_capture_and_holds_no_buffers() {
    let path = socket("alone");
    let service = CameraService::new(camera("cam")).serve(&path).unwrap();

    let controls = ControlClient::connect(&path).unwrap();
    // Remembered while nothing streams, applied when a capture starts.
    let applied = controls.set_exposure_us(2000).unwrap();
    assert!(applied.deferred, "{applied:?}");
    assert_eq!(
        controls.get_control(EXPOSURE).unwrap(),
        ControlValue::Uint(2000)
    );
    std::thread::sleep(Duration::from_millis(200));
    let stats = service.stats();
    assert_eq!((stats.clients, stats.sent, stats.restarts), (0, 0, 0));
    assert_eq!(service.plan(), None, "a control client started a capture");
    assert!(service.metrics().client_metrics.is_empty());
    assert!(service.cameras().iter().all(|c| !c.in_use));

    // A frame client then starts the capture once, with the control in effect.
    let frames = FrameClient::request(&path, &Frames::nv12()).unwrap();
    drop(frame(&frames));
    assert_eq!(
        frames.get_control(EXPOSURE).unwrap(),
        ControlValue::Uint(2000)
    );
    drop(controls);
    std::thread::sleep(Duration::from_millis(200));
    drop(frame(&frames));
    assert_eq!(service.stats().restarts, 0);
}

#[test]
fn the_policy_applies_to_control_clients() {
    let path = socket("policy");
    let _service = CameraService::new(camera("cam"))
        .keep_streaming()
        .control_policy(ControlPolicy::owner_only())
        .serve(&path)
        .unwrap();
    let owner = FrameClient::request(&path, &Frames::nv12()).unwrap();
    let controls = ControlClient::connect(&path).unwrap();
    // Never the owner, even alone on the camera; reading and listing are allowed.
    assert!(matches!(
        refusal(controls.set_exposure_us(500)),
        ControlRefusal::NotPermitted(_)
    ));
    assert!(owner.set_exposure_us(600).is_ok());
    assert_eq!(
        controls.get_control(EXPOSURE).unwrap(),
        ControlValue::Uint(600)
    );
    assert!(controls.controls().unwrap().iter().all(|d| !d.writable));
    drop(owner);
    std::thread::sleep(Duration::from_millis(300));
    assert!(matches!(
        refusal(controls.set_exposure_us(500)),
        ControlRefusal::NotPermitted(_)
    ));

    // A camera the service does not have.
    assert!(matches!(
        ControlClient::connect_camera(&path, "nope"),
        Err(IpcError::ControlRefused(_))
    ));
}

#[test]
fn a_nonblocking_client_delivers_once_the_service_appears() {
    let path = socket("later");
    let _ = std::fs::remove_file(&path);
    let started = Instant::now();
    let client = FrameClient::options(&path)
        .camera("right")
        .reconnecting()
        .request_nonblocking(&Frames::nv12())
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(100), "blocked");
    assert!(!client.is_connected());
    assert!(client.last_error().is_some());
    // Not ready, not an error; neither blocks.
    for _ in 0..3 {
        readable(client.as_raw_fd(), 150);
        let started = Instant::now();
        assert!(matches!(client.try_next(), RecvOutcome::Empty));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "try_next blocked"
        );
    }
    let started = Instant::now();
    assert!(matches!(
        client.recv(Duration::from_millis(100)),
        RecvOutcome::Empty
    ));
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "recv overran its wait"
    );

    let _service = serve(&path);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "never delivered");
        readable(client.as_raw_fd(), 500);
        match client.try_next() {
            RecvOutcome::Data(frame) => {
                assert_eq!(frame.meta().format.code, FourCc::NV12);
                break;
            }
            RecvOutcome::Empty => {}
            RecvOutcome::Closed => panic!("closed"),
        }
    }
    assert!(client.is_connected() && client.delivered().is_some());
    assert!(client.last_error().is_none());
    // The first connection is not a reconnection.
    assert_eq!(client.reconnects(), 0);
}

#[test]
fn readiness_is_awaited_and_a_client_that_does_not_reconnect_gives_up() {
    let path = socket("ready");
    let _ = std::fs::remove_file(&path);
    let frames = FrameClient::options(&path)
        .request_nonblocking(&Frames::nv12())
        .unwrap();
    let controls = ControlClient::options(&path)
        .reconnecting()
        .controls_nonblocking()
        .unwrap();
    assert!(matches!(controls.try_event(), RecvOutcome::Empty));
    let serving = std::thread::spawn({
        let path = path.clone();
        move || {
            std::thread::sleep(Duration::from_millis(300));
            serve(&path)
        }
    });
    block_on(frames.ready()).unwrap();
    block_on(controls.ready()).unwrap();
    assert!(matches!(block_on(frames.next()), RecvOutcome::Data(_)));
    frames.set_exposure_us(1234).unwrap();
    let RecvOutcome::Data(event) = block_on(controls.next_event()) else {
        panic!("no event");
    };
    assert_eq!(
        (event.value, event.by),
        (ControlValue::Uint(1234), frames.client_id())
    );

    // The service goes and comes back: the reconnecting control client subscribes again.
    drop(serving.join().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(controls.try_event(), RecvOutcome::Empty) || controls.is_connected() {
        assert!(Instant::now() < deadline, "never noticed");
        readable(controls.as_raw_fd(), 100);
    }
    let service = serve(&path);
    block_on(controls.ready()).unwrap();
    assert_eq!(controls.reconnects(), 1);
    assert_eq!(service.stats().clients, 0);
    drop(service);

    // Not reconnecting: gives up after its timeout (the service is gone), or at once when
    // refused.
    let gone = socket("gone");
    let _ = std::fs::remove_file(&gone);
    let quick = FrameClient::options(&gone)
        .timeout(Duration::from_millis(300))
        .request_nonblocking(&Frames::nv12())
        .unwrap();
    let started = Instant::now();
    loop {
        assert!(started.elapsed() < Duration::from_secs(3), "never gave up");
        readable(quick.as_raw_fd(), 200);
        if matches!(quick.try_next(), RecvOutcome::Closed) {
            break;
        }
    }
    assert!(readable(quick.as_raw_fd(), 0), "closed but not readable");
    assert!(block_on(quick.ready()).is_err());
    assert!(quick.last_error().is_some());

    let path = socket("refused");
    let _service = serve(&path);
    let refused = FrameClient::options(&path)
        .camera("nope")
        .request_nonblocking(&Frames::nv12())
        .unwrap();
    assert!(matches!(
        block_on(refused.ready()),
        Err(IpcError::Rejected(_))
    ));
    assert!(matches!(refused.try_next(), RecvOutcome::Closed));
    let refused = ControlClient::options(&path)
        .camera("nope")
        .controls_nonblocking()
        .unwrap();
    assert!(matches!(
        block_on(refused.ready()),
        Err(IpcError::ControlRefused(_))
    ));
}
