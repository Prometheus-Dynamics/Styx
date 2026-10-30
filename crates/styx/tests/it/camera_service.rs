//! A camera served to clients in other processes (here: other threads, over the same Unix
//! sockets): C270 MJPEG frames replayed as a camera, requested by a luma detector and an RGB
//! consumer, delivered through memfds without copying.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient, IpcError};
use styx::prelude::*;

use crate::support::c270::c270_recording;

fn camera(recording: &std::path::Path) -> ProbedDevice {
    CaptureRequest::replay_source(
        ReplaySourceConfig::new(recording)
            .pacing(ReplayPacing::Realtime)
            .loop_forever(true),
    )
    .unwrap()
    .into_device()
}

fn frame(client: &FrameClient) -> FrameLease {
    for _ in 0..50 {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(200)) {
            return frame;
        }
    }
    panic!("no frame");
}

#[test]
fn clients_ask_for_frames_and_share_one_capture() {
    let recording = c270_recording("service");
    let socket = std::env::temp_dir().join(format!("styx-service-{}.sock", std::process::id()));
    let service = CameraService::new(camera(&recording))
        .keep_streaming()
        .serve(&socket)
        .unwrap();

    let detector = FrameClient::request(&socket, &Frames::gray().size(320, 180)).unwrap();
    assert!(
        detector.plan().unwrap().contains("turbojpeg-luma"),
        "{:?}",
        detector.plan()
    );
    let small = frame(&detector);
    assert_eq!(small.meta().format.code, FourCc::GREY);
    assert_eq!(small.meta().format.resolution.width.get(), 320);

    // A second client fits the running capture: it joins without a restart.
    let rgb =
        FrameClient::request(&socket, &Frames::formats([FourCc::RG24]).size(640, 360)).unwrap();
    let colour = frame(&rgb);
    assert_eq!(colour.meta().format.code, FourCc::RG24);
    assert_eq!(colour.meta().format.resolution.width.get(), 640);
    let stats = service.stats();
    assert_eq!(stats.clients, 2);
    assert_eq!(stats.restarts, 0);
    // Decoded straight into memfds: nothing copied on the way out.
    assert_eq!(stats.copied, 0, "{stats:?}");
    drop((small, colour));

    // The region applies to the detector's next frames (decoded at 1/4: 640x360 -> 160x90).
    detector
        .set_roi(Some(FrameRect::new(0, 0, 640, 360)))
        .unwrap();
    let mut width = 0;
    for _ in 0..20 {
        width = frame(&detector).meta().format.resolution.width.get();
        if width == 160 {
            break;
        }
    }
    assert_eq!(width, 160);

    // Frames the camera cannot give are refused with the planner's reasons.
    match FrameClient::request(&socket, &Frames::formats([FourCc::new(*b"XVID")])) {
        Err(IpcError::Rejected(reason)) => assert!(reason.contains("no capture mode"), "{reason}"),
        other => panic!("expected a rejection, got {:?}", other.map(|c| c.plan())),
    }
    assert_eq!(service.stats().rejected, 1);

    drop(rgb);
    for _ in 0..50 {
        if service.stats().clients == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(service.stats().clients, 1);
    frame(&detector);

    service.stop();
    assert!(matches!(
        detector.recv(Duration::from_secs(1)),
        RecvOutcome::Closed | RecvOutcome::Empty
    ));
    let _ = std::fs::remove_file(recording);
}

#[test]
fn clients_come_and_go_concurrently() {
    let recording = c270_recording("service-churn");
    let socket = std::env::temp_dir().join(format!("styx-churn-{}.sock", std::process::id()));
    let service = CameraService::new(camera(&recording))
        .stop_when_idle(Duration::from_millis(200))
        .serve(&socket)
        .unwrap();
    let workers: Vec<_> = (0..4)
        .map(|i| {
            let socket = socket.clone();
            std::thread::spawn(move || {
                let req = if i % 2 == 0 {
                    Frames::gray().size(320, 180)
                } else {
                    Frames::formats([FourCc::RG24]).size(320, 180)
                };
                for _ in 0..5 {
                    let client = FrameClient::request(&socket, &req).unwrap();
                    let _held = (frame(&client), frame(&client));
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    for _ in 0..50 {
        if service.stats().clients == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let stats = service.stats();
    assert_eq!(stats.clients, 0, "{stats:?}");
    assert!(stats.sent >= 40, "{stats:?}");
    // The camera went with the last client.
    assert!(service.plan().is_none());
    drop(service);
    let _ = std::fs::remove_file(recording);
}

/// Join and first frame, with a quiet client connected. Old worst case: the join's capture
/// restart waited for the quiet client's frames lock, which a client held for a whole
/// `FRAME_WAIT` (50 ms) per wait; here a 10 fps camera makes most waits run out. The bound
/// (300 ms) is about six frame intervals plus the restart, so it holds on a loaded CI machine.
const JOIN_BOUND: Duration = Duration::from_millis(300);

/// Dropping a client until the service counts it gone. The service notices a closed socket
/// between its waits for frames (at most one `FRAME_WAIT` away); 100 ms leaves room for that
/// and for a loaded machine.
const LEAVE_BOUND: Duration = Duration::from_millis(100);

#[test]
fn a_quiet_client_does_not_hold_up_a_join_or_a_leave() {
    let camera = VirtualSourceConfig::new()
        .name("virtual-slow")
        .format(FourCc::RG24)
        .resolution(320, 180)
        .fps(10)
        .into_device();
    let socket = socket_path("quiet");
    // No spare captures: every join restarts the capture, which takes each client's frames.
    let service = CameraService::new(camera)
        .keep_streaming()
        .spare_clients(0)
        .serve(&socket)
        .unwrap();
    let request = Frames::formats([FourCc::RG24]).size(320, 180);

    // Connected, and never reads a frame.
    let quiet = FrameClient::request(&socket, &request).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let start = Instant::now();
    let joiner = FrameClient::request(&socket, &request).unwrap();
    let RecvOutcome::Data(_) = joiner.recv(Duration::from_secs(2)) else {
        panic!("the joining client got no frame");
    };
    let join = start.elapsed();
    assert!(join < JOIN_BOUND, "join and first frame took {join:?}");

    let third = FrameClient::request(&socket, &request).unwrap();
    assert_eq!(service.stats().clients, 3);
    let start = Instant::now();
    drop(third);
    while service.stats().clients != 2 && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let leave = start.elapsed();
    assert_eq!(service.stats().clients, 2);
    assert!(leave < LEAVE_BOUND, "leave took {leave:?}");
    drop((quiet, joiner));
}

#[cfg(feature = "async")]
#[tokio::test(flavor = "multi_thread")]
async fn frames_can_be_awaited() {
    let recording = c270_recording("service-async");
    let socket = std::env::temp_dir().join(format!("styx-async-{}.sock", std::process::id()));
    let _service = CameraService::new(camera(&recording))
        .keep_streaming()
        .serve(&socket)
        .unwrap();
    let client = FrameClient::request(&socket, &Frames::gray().size(320, 180)).unwrap();
    for _ in 0..3 {
        let RecvOutcome::Data(frame) = client.recv_async().await else {
            panic!("no frame");
        };
        assert_eq!(frame.meta().format.resolution.width.get(), 320);
    }

    // Planned frames in this process, too, on a shared capture.
    let plan = styx::planner::plan_many(
        &camera(&recording),
        &[
            Frames::gray().size(320, 180),
            Frames::formats([FourCc::RG24]),
        ],
    )
    .unwrap();
    let mut consumers = plan.start().unwrap();
    let mut rgb = consumers.pop().unwrap();
    let mut luma = consumers.pop().unwrap();
    for _ in 0..3 {
        let RecvOutcome::Data(small) = luma.next_frame_async().await else {
            panic!("no luma frame");
        };
        let RecvOutcome::Data(full) = rgb.next_frame_async().await else {
            panic!("no rgb frame");
        };
        assert_eq!(small.meta().format.code, FourCc::GREY);
        assert_eq!(full.meta().format.code, FourCc::RG24);
    }
    let _ = std::fs::remove_file(recording);
}

fn virtual_camera(name: &str) -> ProbedDevice {
    VirtualSourceConfig::new()
        .name(name)
        .format(FourCc::RG24)
        .resolution(320, 180)
        .fps(60)
        .into_device()
}

fn socket_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("styx-{name}-{}.sock", std::process::id()))
}

#[test]
fn one_service_serves_several_cameras() {
    let socket = socket_path("cameras");
    let service = CameraService::with_cameras(vec![
        virtual_camera("virtual-front"),
        virtual_camera("virtual-back"),
    ])
    .keep_streaming()
    .serve(&socket)
    .unwrap();
    let cameras = FrameClient::cameras(&socket).unwrap();
    let names: Vec<&str> = cameras.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["virtual-front", "virtual-back"]);
    assert!(cameras.iter().all(|c| !c.in_use));

    let rgb = Frames::formats([FourCc::RG24]);
    let back = FrameClient::request_camera(&socket, "back", &rgb).unwrap();
    assert!(back.plan().unwrap().contains("virtual-back"));
    let front = FrameClient::request(&socket, &rgb).unwrap();
    assert!(front.plan().unwrap().contains("virtual-front"));
    frame(&back);
    frame(&front);
    assert!(
        FrameClient::cameras(&socket)
            .unwrap()
            .iter()
            .all(|c| c.in_use)
    );

    match FrameClient::request_camera(&socket, "side", &rgb) {
        Err(IpcError::Rejected(reason)) => assert!(reason.contains("no camera named side")),
        other => panic!("expected a rejection, got {:?}", other.map(|c| c.plan())),
    }
    assert_eq!(service.stats().clients, 2);
}

#[test]
fn the_service_refuses_what_it_should_not_serve() {
    let socket = socket_path("limits");
    let service = CameraService::new(virtual_camera("virtual-limits"))
        .max_clients(1)
        .serve(&socket)
        .unwrap();
    let rgb = Frames::formats([FourCc::RG24]);
    let greedy = rgb.clone().every_frame(100_000);
    match FrameClient::request(&socket, &greedy) {
        Err(IpcError::Rejected(reason)) => assert!(reason.contains("queued frames"), "{reason}"),
        other => panic!("expected a rejection, got {:?}", other.map(|c| c.plan())),
    }
    let _first = FrameClient::request(&socket, &rgb).unwrap();
    match FrameClient::request(&socket, &rgb) {
        Err(IpcError::Rejected(reason)) => assert!(reason.contains("already serves 1"), "{reason}"),
        other => panic!("expected a rejection, got {:?}", other.map(|c| c.plan())),
    }
    assert_eq!(service.stats().rejected, 2);
    drop(service);

    // Only processes the service trusts get an answer.
    let socket = socket_path("trust");
    let service = CameraService::new(virtual_camera("virtual-trust"))
        .authorize(|peer| peer.uid == u32::MAX)
        .serve(&socket)
        .unwrap();
    assert!(FrameClient::request(&socket, &rgb).is_err());
    assert_eq!(service.stats().unauthorized, 1);
    let socket = socket_path("trusted");
    let _trusting = CameraService::new(virtual_camera("virtual-trusted"))
        .authorize(|peer| peer.pid == std::process::id() as i32)
        .serve(&socket)
        .unwrap();
    assert!(FrameClient::request(&socket, &rgb).is_ok());
}

#[test]
fn a_reconnecting_client_survives_a_service_restart() {
    let socket = socket_path("restart");
    let camera = virtual_camera("virtual-restart");
    let service = CameraService::new(camera.clone())
        .keep_streaming()
        .serve(&socket)
        .unwrap();
    let client = FrameClient::request(&socket, &Frames::formats([FourCc::RG24]))
        .unwrap()
        .reconnecting();
    frame(&client);
    service.stop();
    // Gone: receives come back empty instead of closed.
    let mut closed = false;
    for _ in 0..5 {
        closed |= matches!(client.recv(Duration::from_millis(50)), RecvOutcome::Closed);
    }
    assert!(!closed);
    assert!(!client.is_connected());

    let _service = CameraService::new(camera)
        .keep_streaming()
        .serve(&socket)
        .unwrap();
    frame(&client);
    assert!(client.is_connected());
    assert_eq!(client.reconnects(), 1);
}

#[test]
fn clients_learn_what_they_get_and_strict_ones_are_refused_less() {
    let socket = socket_path("delivered");
    let _service = CameraService::new(virtual_camera("virtual-delivered"))
        .keep_streaming()
        .serve(&socket)
        .unwrap();
    // The virtual camera cannot scale RGB: frames come at its size, and the client is told.
    let small = FrameRequest::formats([FourCc::RG24]).size(160, 90);
    let client = FrameClient::request(&socket, &small).unwrap();
    let delivered = client.delivered().unwrap();
    assert_eq!(delivered.format, FourCc::RG24);
    assert_eq!(delivered.size, (320, 180));
    assert_eq!(
        delivered.unmet,
        [styx::planner::Unmet::Size {
            wanted: (160, 90),
            delivered: (320, 180),
        }]
    );
    let first = frame(&client);
    let res = first.meta().format.resolution;
    assert_eq!((res.width.get(), res.height.get()), delivered.size);

    match FrameClient::request(&socket, &small.clone().strict()) {
        Err(IpcError::Rejected(reason)) => assert!(reason.contains("strict"), "{reason}"),
        other => panic!("expected a refusal, got {:?}", other.map(|c| c.plan())),
    }
}

#[test]
fn opening_gives_up_at_its_timeout() {
    // A socket that takes connections but never answers them.
    let socket = socket_path("silent");
    let _silent = styx::ipc::FrameServer::bind(&socket).unwrap();
    let started = std::time::Instant::now();
    let result = FrameClient::options(&socket)
        .timeout(Duration::from_millis(200))
        .request(&FrameRequest::formats([FourCc::RG24]));
    let took = started.elapsed();
    match result {
        Err(IpcError::Io(err)) => assert_eq!(err.kind(), std::io::ErrorKind::TimedOut),
        other => panic!("expected a timeout, got {:?}", other.map(|c| c.plan())),
    }
    assert!(
        took >= Duration::from_millis(200) && took < Duration::from_secs(2),
        "{took:?}"
    );
}

#[cfg(feature = "async")]
#[tokio::test]
async fn opening_asynchronously_can_time_out_or_be_dropped() {
    let socket = socket_path("silent-async");
    let _silent = styx::ipc::FrameServer::bind(&socket).unwrap();
    let options = FrameClient::options(&socket).timeout(Duration::from_millis(150));
    let rgb = FrameRequest::formats([FourCc::RG24]);
    match options.request_async(&rgb).await {
        Err(IpcError::Io(err)) => assert_eq!(err.kind(), std::io::ErrorKind::TimedOut),
        other => panic!("expected a timeout, got {:?}", other.map(|c| c.plan())),
    }
    // Shutting down: the caller drops the open while it waits.
    let long = FrameClient::options(&socket).timeout(Duration::from_secs(30));
    let dropped = tokio::time::timeout(Duration::from_millis(50), long.request_async(&rgb)).await;
    assert!(dropped.is_err());

    // And against a real service, it opens.
    let served = socket_path("served-async");
    let _service = CameraService::new(virtual_camera("virtual-async"))
        .keep_streaming()
        .serve(&served)
        .unwrap();
    let client = FrameClient::options(&served)
        .request_async(&rgb)
        .await
        .unwrap();
    assert_eq!(client.delivered().unwrap().format, FourCc::RG24);
}

#[test]
fn a_replay_camera_is_played_through_the_service_controls() {
    use styx::capture_api::replay_controls::ids;
    let recording = c270_recording("service-playback");
    let socket =
        std::env::temp_dir().join(format!("styx-service-playback-{}.sock", std::process::id()));
    let device = CaptureRequest::replay_source(
        ReplaySourceConfig::new(&recording)
            .pacing(ReplayPacing::Realtime)
            .hold_at_end(true),
    )
    .unwrap()
    .into_device();
    let service = CameraService::new(device)
        .keep_streaming()
        .serve(&socket)
        .unwrap();
    let client = FrameClient::request(&socket, &Frames::any()).unwrap();
    frame(&client);
    // The playback controls are listed with the camera's and work as camera controls.
    let names: Vec<String> = client
        .controls()
        .unwrap()
        .into_iter()
        .map(|d| d.meta.name)
        .collect();
    assert!(names.iter().any(|n| n == "replay_seek_frame"), "{names:?}");
    client
        .set_control(ids::REPLAY_PLAYING, ControlValue::Bool(false))
        .unwrap();
    client
        .set_control(ids::REPLAY_SEEK_FRAME, ControlValue::Uint(1))
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        client.get_control(ids::REPLAY_FRAME).unwrap(),
        ControlValue::Uint(1)
    );
    assert_eq!(
        client.get_control(ids::REPLAY_STATE).unwrap(),
        ControlValue::Int(1)
    );
    drop(client);
    drop(service);
    let _ = std::fs::remove_file(recording);
}
