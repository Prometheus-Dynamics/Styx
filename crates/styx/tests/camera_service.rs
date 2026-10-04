//! A camera served to clients in other processes (here: other threads, over the same Unix
//! sockets): C270 MJPEG frames replayed as a camera, requested by a luma detector and an RGB
//! consumer, delivered through memfds without copying.

#![cfg(all(
    target_os = "linux",
    feature = "codec-turbojpeg",
    feature = "replay-mcap"
))]

use std::path::PathBuf;
use std::time::Duration;

use styx::DeviceIdentity;
use styx::ipc::{CameraService, FrameClient, IpcError};
use styx::prelude::*;

const FIXTURE: &[u8] = include_bytes!("../../../testing/fixtures/c270_720p_rst.mjpeg");

/// The C270 fixture as a replayable MJPEG 1280x720 camera.
fn c270_recording(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("styx-{name}-{}.mcap", std::process::id()));
    let res = Resolution::new(1280, 720).unwrap();
    let format = MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb);
    let header = RecordingHeader {
        device: DeviceIdentity {
            display: "c270 fixture".into(),
            keys: vec!["fixture:c270".into()],
        },
        backend: "v4l2".into(),
        format,
        interval: Interval::from_fps(30),
    };
    let mut recorder = StreamRecorder::with_header(&path, &header).unwrap();
    let starts: Vec<usize> = (0..FIXTURE.len() - 2)
        .filter(|&i| FIXTURE[i..i + 3] == [0xFF, 0xD8, 0xFF])
        .collect();
    for (k, &start) in starts.iter().enumerate() {
        let jpeg = &FIXTURE[start..*starts.get(k + 1).unwrap_or(&FIXTURE.len())];
        let mut buf = BufferPool::with_limits(1, jpeg.len(), 0).lease();
        buf.resize(jpeg.len());
        buf.as_mut_slice().copy_from_slice(jpeg);
        let meta = FrameMeta::new(format, k as u64 * 33_000_000);
        recorder
            .record(&FrameLease::single_plane(meta, buf, jpeg.len(), jpeg.len()))
            .unwrap();
    }
    recorder.finish().unwrap();
    path
}

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
