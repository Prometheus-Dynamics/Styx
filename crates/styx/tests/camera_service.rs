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

    let detector = FrameClient::request(
        &socket,
        &FrameRequirements::luma().output_resolution(320, 180),
    )
    .unwrap();
    assert!(
        detector.plan().unwrap().contains("turbojpeg-luma"),
        "{:?}",
        detector.plan()
    );
    let small = frame(&detector);
    assert_eq!(small.meta().format.code, FourCc::GREY);
    assert_eq!(small.meta().format.resolution.width.get(), 320);

    // A second client fits the running capture: it joins without a restart.
    let rgb = FrameClient::request(
        &socket,
        &FrameRequirements::formats([FourCc::RG24]).output_resolution(640, 360),
    )
    .unwrap();
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
    match FrameClient::request(&socket, &FrameRequirements::formats([FourCc::H264])) {
        Err(IpcError::Rejected(reason)) => assert!(reason.contains("no capture mode"), "{reason}"),
        other => panic!(
            "expected a rejection, got {:?}",
            other.map(|c| c.plan().map(str::to_owned))
        ),
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
                    FrameRequirements::luma().output_resolution(320, 180)
                } else {
                    FrameRequirements::formats([FourCc::RG24]).output_resolution(320, 180)
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
    let client = FrameClient::request(
        &socket,
        &FrameRequirements::luma().output_resolution(320, 180),
    )
    .unwrap();
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
            FrameRequirements::luma().output_resolution(320, 180),
            FrameRequirements::formats([FourCc::RG24]),
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
