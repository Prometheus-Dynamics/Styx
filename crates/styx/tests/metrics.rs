//! Per-camera metrics of running captures: a single capture, a shared capture with a slow
//! consumer, and a camera service asked for its metrics by another client.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use styx::ipc::{CameraService, FrameClient};
use styx::prelude::*;

fn virtual_camera(name: &str, fps: u32) -> ProbedDevice {
    VirtualSourceConfig::new()
        .name(name)
        .format(FourCc::RG24)
        .resolution(160, 90)
        .fps(fps)
        .into_device()
}

#[test]
fn a_capture_reports_frames_rate_latency_and_cpu() {
    let device = virtual_camera("metrics-single", 60);
    // Monotonic timestamps (a virtual camera's are stream-relative), so latency is measurable.
    let handle = CaptureRequest::new(&device)
        .config(StyxConfig::new().timestamp_clock(ClockSource::Monotonic))
        .start()
        .unwrap();
    let mut received = 0;
    while received < 40 {
        if let RecvOutcome::Data(_) = handle.recv_blocking(Duration::from_millis(500)) {
            received += 1;
        }
    }
    let m = handle.camera_metrics();
    assert_eq!(m.name, "metrics-single");
    assert_eq!(m.backend, "virtual");
    assert_eq!(m.frames.received, 40);
    assert!(m.frames.captured >= 40 && m.frames.delivered >= 40, "{m:?}");
    assert!((m.fps.configured.unwrap() - 60.0).abs() < 0.5, "{m:?}");
    let fps = m.fps.measured.unwrap();
    assert!((30.0..80.0).contains(&fps), "{fps}");
    assert_eq!(m.latency.sensor_to_receive.samples, 40);
    assert!(m.latency.sensor_to_delivery.samples >= 40);
    assert_eq!(m.cpu.threads, 1);
    assert_eq!(m.drops.total, 0);
    // Listed process-wide while it runs, and gone after.
    let all = styx::metrics::snapshot();
    assert!(all.cameras.iter().any(|c| c.id == m.id));
    assert!(all.process.cameras_open >= 1);
    assert!(all.prometheus_text().contains("camera=\"metrics-single\""));
    handle.stop();
    assert!(
        !styx::metrics::snapshot()
            .cameras
            .iter()
            .any(|c| c.id == m.id)
    );
}

#[test]
fn a_slow_consumer_of_a_shared_capture_is_the_one_that_drops() {
    let device = virtual_camera("metrics-shared", 120);
    let req = FrameRequirements::formats([FourCc::RG24]);
    let plan = styx::planner::plan_many(&device, &[req.clone(), req]).unwrap();
    let mut consumers = plan.start().unwrap();
    let mut slow = consumers.pop().unwrap();
    let mut fast = consumers.pop().unwrap();
    let start = Instant::now();
    let mut slow_frames = 0;
    while start.elapsed() < Duration::from_millis(1200) {
        let _ = fast.next_frame(Duration::from_millis(50));
        // The slow one takes a frame every 100 ms.
        if start.elapsed().as_millis() / 100 > slow_frames {
            if let RecvOutcome::Data(_) = slow.next_frame(Duration::ZERO) {}
            slow_frames += 1;
        }
    }
    let camera = styx::metrics::snapshot()
        .cameras
        .into_iter()
        .find(|c| c.name == "metrics-shared")
        .expect("the shared capture is listed");
    assert_eq!(camera.consumers.len(), 2, "{camera:?}");
    let mut by_drops = camera.consumers.clone();
    by_drops.sort_by_key(|c| c.dropped);
    let (fast_m, slow_m) = (&by_drops[0], &by_drops[1]);
    assert!(fast_m.received > 3 * slow_m.received, "{camera:?}");
    assert!(slow_m.dropped > 50, "{camera:?}");
    assert!(fast_m.dropped <= 2, "{camera:?}");
}

#[test]
fn the_camera_service_reports_its_metrics_to_other_clients() {
    let socket = std::env::temp_dir().join(format!("styx-metrics-{}.sock", std::process::id()));
    let service = CameraService::new(virtual_camera("metrics-service", 60))
        .keep_streaming()
        .serve(&socket)
        .unwrap();
    let client =
        FrameClient::request(&socket, &FrameRequirements::formats([FourCc::RG24])).unwrap();
    let mut got = 0;
    while got < 10 {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(500)) {
            drop(frame);
            got += 1;
        }
    }
    // The service takes releases in when it next polls the client.
    std::thread::sleep(Duration::from_millis(100));
    let text = FrameClient::service_metrics_text(&socket).unwrap();
    assert!(text.contains("styx_service_clients 1"), "{text}");
    assert!(text.contains("consumer=\"client 0 pid "), "{text}");
    assert!(text.contains("camera=\"metrics-service\""), "{text}");
    let local = service.metrics();
    assert_eq!(local.client_metrics.len(), 1);
    let c = &local.client_metrics[0];
    assert!(c.received >= 10 && c.hold.samples >= 9, "{c:?}");
    #[cfg(feature = "metrics-serde")]
    {
        let remote = FrameClient::service_metrics(&socket).unwrap();
        assert_eq!(remote.clients, 1);
        assert_eq!(remote.snapshot.process.service_clients, 1);
        assert!(
            remote
                .snapshot
                .cameras
                .iter()
                .any(|c| c.name == "metrics-service" && c.frames.captured > 0)
        );
    }
    drop(client);
    service.stop();
}
