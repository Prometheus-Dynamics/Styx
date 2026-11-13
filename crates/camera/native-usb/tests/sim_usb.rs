//! USB cameras through Lemnos's `camera.frames/1` on the simulated backend, through the real
//! socket: format choice, start, frames, stop, and coming back after the service restarts.

mod sim;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lemnos_host::board::parse;
use lemnos_host::{Options, entry};
use lemnos_sim::SimBoard;
use lemnosd::{Config, Service};
use styx_native_usb::{Client, Phase, Settings, UsbCamera};

use sim::FORMATS;

fn socket_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("styx-native-usb-{}", std::process::id()));
    let dir = dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(name: &str, text: &str) -> (Service, PathBuf) {
    let socket = socket_dir(name).join("sock");
    let config = Config {
        socket: socket.clone(),
        name: "styx-native-usb-test".to_string(),
        board: parse(text).expect("board"),
        drivers: vec![entry::<sim::SimUvc>()],
        backend: Box::new(SimBoard::new()),
        options: Options {
            call_deadline: Duration::from_millis(200),
            ..Options::default()
        },
    };
    (Service::start(config).expect("start lemnosd"), socket)
}

/// Polls the camera until `done` holds or `limit` passes. Returns whether it held.
fn poll_until(
    camera: &mut UsbCamera,
    limit: Duration,
    mut done: impl FnMut(&UsbCamera) -> bool,
) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done(camera) {
            return true;
        }
        drop(camera.next_frame(Duration::from_millis(50)));
    }
    done(camera)
}

#[test]
fn the_best_yuyv_format_streams_with_its_frames_and_stops_by_hand() {
    let (service, socket) = start("stream", &sim::board(&["usb0"]));
    let client = Client::connect(&socket, "styx-native-usb-test").expect("connect");
    let mut camera = UsbCamera::new(client, Settings::default());

    camera.start();
    let status = camera.status();
    assert_eq!(status.phase, Phase::Streaming, "status: {status:?}");
    assert_eq!(status.device.as_deref(), Some("usb0"));
    // 640x480 YUYV at 30 fps: the MJPEG format is refused, 320x240 is smaller.
    assert_eq!(status.format, Some(FORMATS[3]));
    assert_eq!(status.attaches, 1);

    let mut last_id = None;
    let mut last_ts = 0u64;
    let mut seen = 0usize;
    let deadline = Instant::now() + Duration::from_secs(10);
    while seen < 12 && Instant::now() < deadline {
        let Some(lease) = camera.next_frame(Duration::from_millis(200)) else {
            continue;
        };
        assert_eq!(lease.format().resolution.width.get(), 640);
        assert_eq!(lease.format().resolution.height.get(), 480);
        assert_ne!(Some(lease.id()), last_id, "identity repeated");
        assert!(lease.timestamp_ns() > last_ts, "timestamps not increasing");
        last_id = Some(lease.id());
        last_ts = lease.timestamp_ns();
        let plane = lease
            .read()
            .expect("mapped for the CPU")
            .plane(0)
            .expect("plane 0");
        assert_eq!(plane.data.len(), 640 * 480 * 2);
        seen += 1;
    }
    assert!(seen >= 12, "frames seen: {seen}");
    assert!(camera.status().frames >= 12);

    camera.stop();
    let status = camera.status();
    assert_eq!(status.phase, Phase::Off, "status: {status:?}");
    assert_eq!(status.format, None);
    // A stopped camera does not restart on its own.
    assert!(camera.next_frame(Duration::from_millis(300)).is_none());
    assert_eq!(camera.status().phase, Phase::Off);

    drop(camera);
    service.stop();
}

#[test]
fn a_camera_that_is_absent_is_retried_until_it_arrives() {
    // No service yet: the client connects lazily, and the camera is wanted under a name the first
    // board does not have.
    let dir = socket_dir("absent");
    let socket = dir.join("sock");
    let client = Client::connecting(&socket, "styx-native-usb-test");
    let mut camera = UsbCamera::new(
        client,
        Settings {
            device: Some("usb-late".to_string()),
            ..Settings::default()
        },
    );

    camera.start();
    assert_eq!(camera.status().phase, Phase::Waiting);
    assert!(camera.status().last_error.is_some());
    // Each attempt waits on the connection, so the failures come slower than the schedule alone.
    let waited = poll_until(&mut camera, Duration::from_secs(4), |c| {
        c.status().failures >= 2
    });
    assert!(waited, "status: {:?}", camera.status());
    let status = camera.status();
    assert_eq!(status.phase, Phase::Waiting);
    assert!(
        status.retry_in.is_some_and(|d| d <= Duration::from_secs(5)),
        "retry wait is capped: {status:?}"
    );

    // The service comes up with the camera on the board.
    let (service, _) = start("absent", &sim::board(&["usb0", "usb-late"]));
    let streaming = poll_until(&mut camera, Duration::from_secs(20), |c| {
        c.status().phase == Phase::Streaming
    });
    assert!(streaming, "status: {:?}", camera.status());
    let status = camera.status();
    assert_eq!(status.device.as_deref(), Some("usb-late"));
    assert_eq!(status.failures, 0, "a success clears the failures");
    assert!(status.last_error.is_none());
    assert!(camera.next_frame(Duration::from_secs(5)).is_some());

    camera.stop();
    drop(camera);
    service.stop();
}

#[test]
fn a_camera_lost_with_its_service_is_attached_again_when_it_returns() {
    let (service, socket) = start("replug", &sim::board(&["usb0"]));
    let client = Client::connect(&socket, "styx-native-usb-test").expect("connect");
    let mut camera = UsbCamera::new(client, Settings::default());
    camera.start();
    assert_eq!(camera.status().phase, Phase::Streaming);
    assert!(camera.next_frame(Duration::from_secs(5)).is_some());

    // Unplug: the service goes away. The camera notices, and waits.
    service.stop();
    let lost = poll_until(&mut camera, Duration::from_secs(10), |c| {
        c.status().phase == Phase::Waiting
    });
    assert!(lost, "status: {:?}", camera.status());
    assert!(camera.status().last_error.is_some());

    // Replug: the same board comes back, and the camera streams again without being restarted.
    let (service, _) = start("replug", &sim::board(&["usb0"]));
    let back = poll_until(&mut camera, Duration::from_secs(20), |c| {
        c.status().phase == Phase::Streaming
    });
    assert!(back, "status: {:?}", camera.status());
    assert_eq!(camera.status().attaches, 2);
    assert!(camera.next_frame(Duration::from_secs(5)).is_some());

    camera.stop();
    drop(camera);
    service.stop();
}
