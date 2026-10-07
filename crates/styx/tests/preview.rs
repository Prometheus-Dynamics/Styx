//! A preview next to a vision client of the camera service: the vision client's plan, capture
//! and frames do not change when a preview (a low-priority client) joins, even when the
//! preview's own request would have needed another capture mode; a preview that came first
//! gives way when the vision client joins.

#![cfg(all(target_os = "linux", feature = "preview"))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use styx::capture_api::make_virtual_device;
use styx::ipc::{CameraService, CameraServiceHandle, FrameClient};
use styx::prelude::*;
use styx::preview::{JpegBackend, Preview, PreviewConfig};

/// A camera with two RGB modes and no scaler: a preview that must have 640x400 (`strict`)
/// would move the capture to the small mode. (RGB: the virtual camera fills its frames as
/// 3-byte pixels.)
fn camera() -> ProbedDevice {
    let mode = |w, h| {
        Mode::with_interval(
            MediaFormat::new(
                FourCc::RG24,
                Resolution::new(w, h).unwrap(),
                ColorSpace::Srgb,
            ),
            Interval::from_fps(30).unwrap(),
        )
    };
    make_virtual_device("preview-cam", [mode(1280, 800), mode(640, 400)])
}

fn serve(name: &str) -> (CameraServiceHandle, PathBuf) {
    let path =
        std::env::temp_dir().join(format!("styx-preview-{name}-{}.sock", std::process::id()));
    let service = CameraService::new(camera())
        .keep_streaming()
        .serve(&path)
        .unwrap();
    (service, path)
}

/// The vision client's frames: when each arrived, its size, its age.
#[derive(Default)]
struct Seen {
    frames: Vec<(Instant, (u32, u32), Option<Duration>)>,
}

impl Seen {
    fn between(&self, from: Instant, to: Instant) -> Vec<&(Instant, (u32, u32), Option<Duration>)> {
        self.frames
            .iter()
            .filter(|(at, ..)| *at >= from && *at < to)
            .collect()
    }
}

/// Receive on a thread of its own until stopped, as a vision pipeline would.
fn vision(client: FrameClient) -> (Arc<Mutex<Seen>>, Arc<AtomicBool>, JoinHandle<FrameClient>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (seen, stop) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(100)) {
                    let res = frame.meta().format.resolution;
                    let age = frame.meta().latency().total;
                    seen.lock().unwrap().frames.push((
                        Instant::now(),
                        (res.width.get(), res.height.get()),
                        age,
                    ));
                }
            }
            client
        })
    };
    (seen, stop, thread)
}

/// The plan of consumer 0 in a service's plan text (without the note naming who shares its
/// frames).
fn consumer0(plan: &str) -> String {
    let start = plan.find("consumer 0:").expect("consumer 0");
    let rest = &plan[start..];
    let end = rest.find("consumer 1:").unwrap_or(rest.len());
    rest[..end]
        .lines()
        .filter(|line| !line.contains("note: prepared once"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_preview_never_changes_the_vision_clients_capture() {
    if JpegBackend::available().is_empty() {
        return;
    }
    let (service, path) = serve("vision");
    let client = FrameClient::request(&path, &Frames::rgb().size(1280, 800)).unwrap();
    let plan_alone = service.plan().unwrap();
    let (seen, stop, thread) = vision(client);
    std::thread::sleep(Duration::from_millis(1500));
    let before = Instant::now();
    std::thread::sleep(Duration::from_millis(2000));

    // A preview that, as a normal client, would have moved the capture to 640x400.
    let preview = Preview::from_service(
        FrameClient::options(&path),
        PreviewConfig::new()
            .name("vision-test")
            .size(640, 400)
            .max_fps(10.0)
            .with_request(Frames::rgb().size(640, 400).strict()),
    )
    .unwrap();
    let mut viewer = preview.subscribe();
    let first = viewer.recv(Duration::from_secs(10)).unwrap_or_else(|| {
        panic!(
            "no preview frame: {:?} {:?} {:?}",
            preview.metrics(),
            service.stats(),
            service.plan()
        )
    });
    assert_eq!((first.width, first.height), (640, 400));
    assert_eq!(&first.jpeg[..2], &[0xFF, 0xD8]);
    let during = Instant::now();
    let mut previews = 1;
    while during.elapsed() < Duration::from_secs(2) {
        if viewer.recv(Duration::from_millis(500)).is_some() {
            previews += 1;
        }
    }
    let after = Instant::now();
    stop.store(true, Ordering::Release);
    let _client = thread.join().unwrap();

    // The capture was not restarted and the vision client's plan is the same.
    let stats = service.stats();
    assert_eq!(stats.restarts, 0, "{stats:?}");
    assert_eq!(stats.clients, 2);
    let plan_with = service.plan().unwrap();
    assert!(
        plan_with.contains("RG24 1280x800 for 2 consumers"),
        "{plan_with}"
    );
    assert_eq!(consumer0(&plan_alone), consumer0(&plan_with));

    // The vision client's frames: the same size and cadence with the preview as without.
    let seen = seen.lock().unwrap();
    let (without, with) = (
        seen.between(before, during - Duration::from_secs(1)),
        seen.between(during, after),
    );
    assert!(with.iter().all(|(_, size, _)| *size == (1280, 800)));
    let rate =
        |frames: &[&(Instant, (u32, u32), Option<Duration>)], span: f64| frames.len() as f64 / span;
    let (r0, r1) = (
        rate(&without, 1.0),
        rate(&with, (after - during).as_secs_f64()),
    );
    assert!(r0 > 20.0, "vision alone: {r0} fps");
    assert!(
        r1 > r0 * 0.8,
        "vision with the preview: {r1} fps, alone {r0}"
    );
    let gaps = with
        .windows(2)
        .map(|w| w[1].0 - w[0].0)
        .max()
        .unwrap_or_default();
    assert!(gaps < Duration::from_millis(250), "longest gap {gaps:?}");

    // The preview: capped at 10 fps, scaled by itself from the vision client's frames.
    assert!(
        (10..=26).contains(&previews),
        "{previews} preview frames in 2 s"
    );
    let m = preview.metrics();
    assert_eq!(m.source_size, (1280, 800), "{m:?}");
    assert!(m.dropped_rate > 0, "{m:?}");
    assert_eq!(m.errors, 0, "{m:?}");
    drop(preview);
}

#[test]
fn a_preview_that_came_first_gives_way_to_the_vision_client() {
    if JpegBackend::available().is_empty() {
        return;
    }
    let (service, path) = serve("first");
    let preview = Preview::from_service(
        FrameClient::options(&path),
        PreviewConfig::new()
            .size(640, 400)
            .with_request(Frames::rgb().size(640, 400).strict()),
    )
    .unwrap();
    let mut viewer = preview.subscribe();
    viewer.recv(Duration::from_secs(10)).unwrap_or_else(|| {
        panic!(
            "no preview frame: {:?} {:?} {:?}",
            preview.metrics(),
            service.stats(),
            service.plan()
        )
    });
    // Alone, it was planned as it asked.
    assert!(
        service.plan().unwrap().contains("RG24 640x400"),
        "{:?}",
        service.plan()
    );

    // The vision client gets the plan it would get alone (one restart, before its first frame).
    let client = FrameClient::request(&path, &Frames::rgb().size(1280, 800)).unwrap();
    let delivered = client.delivered().unwrap();
    assert_eq!(delivered.size, (1280, 800));
    assert_eq!(service.stats().restarts, 1);
    let plan = service.plan().unwrap();
    assert!(plan.contains("RG24 1280x800 for 2 consumers"), "{plan}");
    let frame = loop {
        if let RecvOutcome::Data(frame) = client.recv(Duration::from_secs(5)) {
            break frame;
        }
    };
    assert_eq!(frame.meta().format.resolution.width.get(), 1280);
    drop(frame);

    // The preview goes on, scaling the vision client's frames itself.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frame = viewer.recv(Duration::from_secs(1));
        if preview.metrics().source_size == (1280, 800) {
            assert!(frame.is_none_or(|f| (f.width, f.height) == (640, 400)));
            break;
        }
        assert!(Instant::now() < deadline, "{:?}", preview.metrics());
    }
}

#[test]
fn an_unwatched_preview_disconnects_and_comes_back() {
    if JpegBackend::available().is_empty() {
        return;
    }
    let (service, path) = serve("idle");
    let preview = Preview::from_service(
        FrameClient::options(&path),
        PreviewConfig::new()
            .size(320, 200)
            .idle_disconnect(Duration::from_millis(300))
            .with_request(Frames::rgb().size(320, 200)),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(service.stats().clients, 0, "connects only when watched");
    let mut viewer = preview.subscribe();
    viewer.recv(Duration::from_secs(10)).unwrap_or_else(|| {
        panic!(
            "no preview frame: {:?} {:?} {:?}",
            preview.metrics(),
            service.stats(),
            service.plan()
        )
    });
    assert_eq!(service.stats().clients, 1);
    drop(viewer);
    let deadline = Instant::now() + Duration::from_secs(10);
    while service.stats().clients != 0 {
        assert!(Instant::now() < deadline, "still connected");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut viewer = preview.subscribe();
    let frame = viewer.recv(Duration::from_secs(10)).expect("frames again");
    assert_eq!((frame.width, frame.height), (320, 200));
}
