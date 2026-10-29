//! A USB 2 camera through the camera service's join (what a client's connection does): an
//! NV12 preview asks `Frames::nv12()`, a vision client asks `Frames::gray()`, and the starts the
//! driver would refuse or stall are scripted (`planner::stalled::script`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Camera, ServiceConfig};
use crate::BackendKind;
use crate::DeviceIdentity;
use crate::capture_api::make_virtual_device;
use crate::ipc::ClientPriority;
use crate::ipc::service::{CameraService, Counters};
use crate::planner::stalled::script;
use crate::planner::{FrameRequest, Frames};
use crate::prelude::{BackendHandle, Mode, ProbedBackend, ProbedDevice};
use styx_capture::prelude::CaptureDescriptor;
use styx_core::prelude::{ColorSpace, FourCc, Interval, MediaFormat, Resolution};

fn mode(code: FourCc, width: u32, height: u32) -> Mode {
    Mode::with_interval(
        MediaFormat::new(
            code,
            Resolution::new(width, height).expect("size"),
            ColorSpace::Srgb,
        ),
        Interval::from_fps(30).expect("rate"),
    )
}

/// A Streaming-Webcam-like USB 2 camera: YUYV and MJPG at 2560x1440, 1920x1080, 1280x720,
/// 800x600 and 640x480, 30 fps, on a 480 Mbit/s bus. `name` identifies it to the scripts.
fn usb2_webcam(name: &str) -> ProbedDevice {
    let mut modes = Vec::new();
    for (w, h) in [
        (2560, 1440),
        (1920, 1080),
        (1280, 720),
        (800, 600),
        (640, 480),
    ] {
        modes.push(mode(FourCc::YUYV, w, h));
        modes.push(mode(FourCc::MJPG, w, h));
    }
    ProbedDevice {
        identity: DeviceIdentity {
            display: name.into(),
            keys: vec![format!("test:{name}")],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::V4l2,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor {
                modes,
                controls: Vec::new(),
            },
            properties: vec![
                ("bus".into(), "usb-xhci-hcd.1-1.1".into()),
                ("usb_speed_mbps".into(), "480".into()),
            ],
        }],
    }
}

fn config() -> ServiceConfig {
    CameraService::new(make_virtual_device("unused", [])).config
}

/// The camera's join for `request` at `priority`: the consumer's plan text, and the camera.
fn join(
    camera: &Camera,
    request: FrameRequest,
    priority: ClientPriority,
) -> Result<String, String> {
    let (_, text, _, _) = camera.join(request, priority, &config(), &Counters::default())?;
    Ok(text)
}

/// The starts of `name` so far, as format and width.
fn starts(name: &str) -> Vec<(FourCc, u32)> {
    script::starts(name)
}

/// A preview (NV12 at 640x400, low priority, alone on its camera) gets MJPG: the NV12 route of
/// the MJPEG mode decodes it, and the uncompressed 640x480 would take 18 MB/s of the bus.
#[test]
fn a_preview_on_a_usb2_camera_joins_mjpg() {
    let name = "join-preview-usb2";
    let camera = Camera::new(usb2_webcam(name));
    let text = join(&camera, Frames::nv12().size(640, 400), ClientPriority::Low)
        .expect("the preview joins");
    assert!(text.contains("MJPG 640x480"), "{text}");
    assert_eq!(starts(name), [(FourCc::MJPG, 640)]);
}

/// A client with no size pinned gets the camera's largest compressed mode, not its largest
/// uncompressed one (YUYV 2560x1440 is 221 MB/s on a 480 Mbit/s bus).
#[test]
fn an_unsized_client_joins_the_largest_mjpg_not_yuyv_2560() {
    let name = "join-unsized-usb2";
    let camera = Camera::new(usb2_webcam(name));
    let text = join(&camera, Frames::nv12(), ClientPriority::Normal).expect("joins");
    assert!(text.contains("MJPG 2560x1440"), "{text}");
    assert_eq!(starts(name), [(FourCc::MJPG, 2560)]);
}

/// The bus refuses MJPG 2560x1440 (ENOSPC): the join falls back to MJPG 1920x1080. It never
/// starts a YUYV mode, and each mode is started once.
#[test]
fn enospc_on_mjpg_2560_falls_back_to_mjpg_1920() {
    let name = "join-enospc-usb2";
    script::refuse(name, FourCc::MJPG, 2560);
    let camera = Camera::new(usb2_webcam(name));
    let text = join(&camera, Frames::nv12(), ClientPriority::Normal).expect("a mode streams");
    assert!(text.contains("MJPG 1920x1080"), "{text}");
    assert_eq!(starts(name), [(FourCc::MJPG, 2560), (FourCc::MJPG, 1920)]);
}

/// MJPG 2560x1440 starts and delivers no frame: the join keeps planning until MJPG 1920x1080
/// streams, rather than walking down the uncompressed modes.
#[test]
fn a_stalled_mjpg_2560_falls_back_to_mjpg_1920() {
    let name = "join-stall-usb2";
    script::stall(name, FourCc::MJPG, 2560);
    let camera = Camera::new(usb2_webcam(name));
    let text = join(&camera, Frames::nv12(), ClientPriority::Normal).expect("a mode streams");
    assert!(text.contains("MJPG 1920x1080"), "{text}");
    assert_eq!(starts(name), [(FourCc::MJPG, 2560), (FourCc::MJPG, 1920)]);
}

/// While a join tries modes (each start slow here), the camera answers its status queries: the
/// start runs with the camera's state unlocked.
#[test]
fn status_is_answered_during_a_fallback() {
    let name = "join-status-usb2";
    script::refuse(name, FourCc::MJPG, 2560);
    script::delay(name, Duration::from_millis(400));
    let camera = Arc::new(Camera::new(usb2_webcam(name)));
    let joining = {
        let camera = camera.clone();
        std::thread::spawn(move || join(&camera, Frames::nv12(), ClientPriority::Normal))
    };
    // Two starts of 400 ms each are in progress.
    std::thread::sleep(Duration::from_millis(200));
    let asked = Instant::now();
    let _ = camera.clients();
    let _ = camera.plan();
    let answered = asked.elapsed();
    assert!(
        answered < Duration::from_millis(100),
        "status took {answered:?} during a start"
    );
    assert!(!joining.is_finished(), "the fallback was still in progress");
    joining
        .join()
        .expect("join thread")
        .expect("the fallback streams");
}

/// A USB camera's mode the supervisor found stalled after its start: a client that joins again
/// gets the next lighter mode, not the same one.
#[test]
fn a_rejoin_after_a_stall_found_later_plans_the_next_mode() {
    let name = "join-rejoin-usb2";
    let camera = Camera::new(usb2_webcam(name));
    let text = join(&camera, Frames::nv12(), ClientPriority::Normal).expect("joins");
    assert!(text.contains("MJPG 2560x1440"), "{text}");
    let mode = camera.device.backends[0]
        .descriptor
        .modes
        .iter()
        .find(|m| m.format.resolution.width.get() == 2560 && m.format.code == FourCc::MJPG)
        .expect("the 2560 mode")
        .clone();
    crate::planner::stalled::record_stall(
        &camera.device,
        BackendKind::V4l2,
        &mode,
        "no frames for 2002 ms",
    );
    let text = join(&camera, Frames::nv12(), ClientPriority::Normal).expect("joins again");
    assert!(text.contains("MJPG 1920x1080"), "{text}");
}

/// A refused camera, joined again and again (its clients reconnect), starts each of its modes
/// once and then waits: the bus refused every start, so the refused modes stay out for their
/// expiry, and the driver is not asked again in a loop.
#[test]
fn a_refused_camera_is_started_once_per_mode_not_in_a_loop() {
    let name = "join-streamon-count";
    let mut device = usb2_webcam(name);
    device.backends[0]
        .properties
        .push(("test-bus".into(), name.into()));
    let modes = device.backends[0].descriptor.modes.len();
    let camera = Camera::new(device);
    let started = Instant::now();
    let mut joins = 0;
    while started.elapsed() < Duration::from_secs(6) {
        let _ = join(&camera, Frames::nv12(), ClientPriority::Normal);
        joins += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    let streamons = script::streamons(name);
    assert!(joins > streamons, "joins were made: {joins}");
    assert!(
        streamons <= modes,
        "{streamons} STREAMONs for {modes} modes"
    );
    assert!(streamons >= 1, "the camera was tried");
}

/// Every lighter mode of a USB camera stalled: the join gives up with the bandwidth error, so
/// the consumers can say that too many cameras share the bus.
#[test]
fn when_no_lighter_mode_streams_the_join_reports_the_bus() {
    let name = "join-exhausted-usb2";
    for width in [2560, 1920, 1280, 800, 640] {
        script::stall(name, FourCc::MJPG, width);
    }
    let camera = Camera::new(usb2_webcam(name));
    let err = join(&camera, Frames::nv12(), ClientPriority::Normal).expect_err("nothing streams");
    assert_eq!(
        err,
        "not enough USB bandwidth on this bus: too many cameras share it (tried MJPG 2560x1440 \
         @30 fps down to MJPG 640x480 @30 fps)"
    );
}
