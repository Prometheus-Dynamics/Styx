//! How USB cameras' modes rank: bus load before CPU cost.

use super::tests::{device, mode, registry};
use super::*;
use crate::prelude::BackendHandle;

/// A USB camera offering YUYV and MJPG at the same sizes, on a bus of `speed_mbps`.
fn uvc_webcam(bus: &str, speed_mbps: Option<&str>) -> ProbedDevice {
    let mut dev = device(
        BackendKind::V4l2,
        BackendHandle::Virtual,
        vec![
            mode(FourCc::YUYV, 640, 480, 30),
            mode(FourCc::YUYV, 320, 240, 30),
            mode(FourCc::MJPG, 640, 480, 30),
            mode(FourCc::MJPG, 320, 240, 30),
        ],
    );
    dev.backends[0].properties = vec![("bus".into(), bus.into())];
    if let Some(s) = speed_mbps {
        dev.backends[0]
            .properties
            .push(("usb_speed_mbps".into(), s.into()));
    }
    dev
}

/// An uncompressed USB mode that takes more than half of a high-speed bus's isochronous
/// bandwidth ranks after the compressed mode of the same size (single and shared plans); a
/// light one, a USB 3 camera's and a camera off USB keep the cheaper uncompressed route.
#[test]
fn heavy_uncompressed_usb_modes_rank_after_mjpg() {
    let code = |dev: &ProbedDevice, w: u32, h: u32| {
        let req = Frames::any().size(w, h).fps(30);
        let single = plan_frames_with(dev, &req, &registry())
            .unwrap()
            .mode
            .format
            .code;
        let shared = plan_many_with(dev, &[req], &registry())
            .unwrap()
            .mode
            .format
            .code;
        assert_eq!(single, shared);
        single
    };
    let usb2 = uvc_webcam("usb-xhci-hcd.1-1.1", Some("480"));
    // YUYV 640x480 at 30 fps: 18.4 MB/s of the 24.6 a high-speed endpoint carries.
    assert_eq!(code(&usb2, 640, 480), FourCc::MJPG);
    // 4.6 MB/s.
    assert_eq!(code(&usb2, 320, 240), FourCc::YUYV);
    // No speed read: high speed assumed.
    assert_eq!(
        code(&uvc_webcam("usb-0000:00:14.0-2", None), 640, 480),
        FourCc::MJPG
    );
    assert_eq!(
        code(&uvc_webcam("usb-0000:00:14.0-2", Some("5000")), 640, 480),
        FourCc::YUYV
    );
    assert_eq!(
        code(&uvc_webcam("platform:vivid-000", None), 640, 480),
        FourCc::YUYV
    );
}

/// A mode that started and delivered no frame is left out for that camera from then on: single
/// and shared plans pick the next mode, and the rejection says why. Other cameras keep it.
#[test]
fn a_mode_that_delivered_no_frames_is_left_out() {
    let mut dev = uvc_webcam("usb-xhci-hcd.1-1.1", Some("480"));
    // Its own identity: the record is process-wide.
    dev.identity.keys = vec!["test:stalled-webcam".into()];
    let req = Frames::any().size(320, 240).fps(30);
    let plan = plan_frames_with(&dev, &req, &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::YUYV);
    let backend = &dev.backends[0];
    assert!(stalled::may_starve(backend, &plan.mode));
    assert!(!stalled::may_starve(backend, &backend.descriptor.modes[3]));

    stalled::mark(
        &dev,
        BackendKind::V4l2,
        &plan.mode.id,
        "no frame in 2000 ms",
    );
    let plan = plan_frames_with(&dev, &req, &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::MJPG);
    let reason = plan
        .rejected
        .iter()
        .find(|r| r.candidate.contains("YUYV 320x240"))
        .map(|r| r.reason.as_str());
    assert_eq!(
        reason,
        Some("delivered no frames when started: no frame in 2000 ms")
    );
    let shared = plan_many_with(&dev, std::slice::from_ref(&req), &registry()).unwrap();
    assert_eq!(shared.mode.format.code, FourCc::MJPG);

    let other = uvc_webcam("usb-xhci-hcd.1-1.2", Some("480"));
    let plan = plan_frames_with(&other, &req, &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::YUYV);
}

/// The fake backend's start: the bus refuses any stream at least 640 wide (ENOSPC, as the driver
/// reports a stream that does not fit), and streams the others.
fn start_on_a_full_bus(
    dev: &ProbedDevice,
    plan: &FramePlan,
) -> Result<(FourCc, u32, u32), crate::capture_api::CaptureError> {
    use crate::capture_api::CaptureError;
    let res = plan.mode.format.resolution;
    let (width, height) = (res.width.get(), res.height.get());
    if width >= 640 {
        return Err(stalled::start_failed(
            dev,
            BackendKind::V4l2,
            &plan.mode,
            CaptureError::Backend("uvc: No space left on device (os error 28)".into()),
        ));
    }
    Ok((plan.mode.format.code, width, height))
}

/// A mode the bus refuses is left out, and the next lower mode of the camera streams: the open
/// reports the mode it got. The refusal is an error the caller can read, not a plan failure.
#[test]
fn a_mode_the_bus_refuses_falls_back_to_a_lower_one() {
    let mut dev = uvc_webcam("usb-xhci-hcd.1-1.1", Some("480"));
    dev.identity.keys = vec!["test:bus-full-webcam".into()];
    let req = Frames::any().fps(30);
    let planned = plan_frames_with(&dev, &req, &registry()).unwrap();
    assert_eq!(
        planned.mode.format.resolution.width.get(),
        640,
        "the largest mode is planned first"
    );

    let (code, width, height) = stalled::open_with_fallback(
        || plan_frames_with(&dev, &req, &registry()),
        |plan| start_on_a_full_bus(&dev, plan),
    )
    .expect("a lower mode streams");
    assert_eq!((width, height), (320, 240), "{code:?}");
    // The refused mode is out of the plans while the bus is full.
    let again = plan_frames_with(&dev, &req, &registry()).unwrap();
    assert_eq!(again.mode.format.resolution.width.get(), 320);
}

/// When no mode fits the bus, the open fails with the bus error, and the modes it refused are
/// out of the plans (each start is tried once, not in a loop).
#[test]
fn when_no_mode_fits_the_bus_error_says_so() {
    use crate::capture_api::CaptureError;
    let mut dev = uvc_webcam("usb-xhci-hcd.1-1.1", Some("480"));
    dev.identity.keys = vec!["test:bus-really-full-webcam".into()];
    let req = Frames::any().fps(30);
    let starts = std::cell::Cell::new(0);
    let err = stalled::open_with_fallback(
        || plan_frames_with(&dev, &req, &registry()),
        |plan| {
            starts.set(starts.get() + 1);
            // Every mode refused.
            Err::<(), _>(stalled::start_failed(
                &dev,
                BackendKind::V4l2,
                &plan.mode,
                CaptureError::Backend("uvc: No space left on device (os error 28)".into()),
            ))
        },
    )
    .expect_err("nothing fits");
    match err {
        crate::planner::OpenError::Capture(CaptureError::NotEnoughBandwidth(msg)) => {
            assert!(msg.contains("lower modes are tried"), "{msg}");
        }
        other => panic!("expected the bus error, got {other:?}"),
    }
    // Each mode was started once (the plans left the refused ones out), not a loop.
    assert!(starts.get() <= 4, "{} starts", starts.get());
}

/// A USB camera offering `modes`, on a bus of `speed_mbps`, with its own identity (the stall and
/// bus records are process-wide).
fn camera_with(name: &str, speed_mbps: &str, modes: Vec<Mode>) -> ProbedDevice {
    let mut dev = device(BackendKind::V4l2, BackendHandle::Virtual, modes);
    dev.backends[0].properties = vec![
        ("bus".into(), "usb-xhci-hcd.1-1.1".into()),
        ("usb_speed_mbps".into(), speed_mbps.into()),
    ];
    dev.identity.keys = vec![format!("test:{name}")];
    dev
}

/// A Streaming-Webcam-like camera: YUYV at 2560x1440 (221 MB/s, more than a high-speed bus
/// carries), MJPG at 1920x1080, and smaller modes of both.
fn webcam_modes() -> Vec<Mode> {
    vec![
        mode(FourCc::YUYV, 2560, 1440, 30),
        mode(FourCc::MJPG, 1920, 1080, 30),
        mode(FourCc::YUYV, 1280, 720, 30),
        mode(FourCc::MJPG, 640, 480, 30),
    ]
}

/// On a USB 2 camera with no pinned size, the compressed mode at a suitable size is planned
/// before the largest uncompressed one: the uncompressed mode is what starves a shared bus.
/// USB 3 cameras, cameras without a compressed mode and cameras whose compressed modes are much
/// smaller keep the largest mode.
#[test]
fn a_usb2_camera_prefers_mjpg_over_a_larger_yuyv_mode() {
    let req = Frames::any().fps(30);
    let usb2 = camera_with("mjpg-pref-usb2", "480", webcam_modes());
    let plan = plan_frames_with(&usb2, &req, &registry()).unwrap();
    assert_eq!(
        (
            plan.mode.format.code,
            plan.mode.format.resolution.width.get()
        ),
        (FourCc::MJPG, 1920),
        "{plan}"
    );
    let shared = plan_many_with(&usb2, std::slice::from_ref(&req), &registry()).unwrap();
    assert_eq!(shared.mode.format.code, FourCc::MJPG);
    assert_eq!(shared.mode.format.resolution.width.get(), 1920);

    // USB 3: the bus carries the uncompressed mode, so the largest one stays first.
    let usb3 = camera_with("mjpg-pref-usb3", "5000", webcam_modes());
    let plan = plan_frames_with(&usb3, &req, &registry()).unwrap();
    assert_eq!(
        (
            plan.mode.format.code,
            plan.mode.format.resolution.width.get()
        ),
        (FourCc::YUYV, 2560)
    );

    // Uncompressed modes only (nothing to prefer): the largest on USB 2.
    let yuyv_only = camera_with(
        "mjpg-pref-yuyv-only",
        "480",
        vec![
            mode(FourCc::YUYV, 2560, 1440, 30),
            mode(FourCc::YUYV, 640, 480, 30),
        ],
    );
    let plan = plan_frames_with(&yuyv_only, &req, &registry()).unwrap();
    assert_eq!(plan.mode.format.resolution.width.get(), 2560);

    // An MJPG mode much smaller than the YUYV one is no suitable size: YUYV stays first.
    let tiny_mjpg = camera_with(
        "mjpg-pref-tiny",
        "480",
        vec![
            mode(FourCc::YUYV, 2560, 1440, 30),
            mode(FourCc::MJPG, 320, 240, 30),
        ],
    );
    let plan = plan_frames_with(&tiny_mjpg, &req, &registry()).unwrap();
    assert_eq!(plan.mode.format.code, FourCc::YUYV);
}

/// A USB 2 camera whose YUYV 2560x1440 stalls: the next plan is MJPG, not the smaller YUYV modes.
#[test]
fn a_stalled_yuyv_2560_falls_back_to_mjpg() {
    let req = Frames::any().fps(30);
    let dev = camera_with(
        "stall-yuyv-2560",
        "480",
        vec![
            mode(FourCc::YUYV, 2560, 1440, 30),
            mode(FourCc::MJPG, 640, 480, 30),
            mode(FourCc::YUYV, 640, 480, 30),
            mode(FourCc::YUYV, 320, 240, 30),
        ],
    );
    let first = plan_frames_with(&dev, &req, &registry()).unwrap();
    assert_eq!(
        (
            first.mode.format.code,
            first.mode.format.resolution.width.get()
        ),
        (FourCc::YUYV, 2560),
        "no compressed mode is a suitable size for it, so it is planned first"
    );
    stalled::mark(
        &dev,
        BackendKind::V4l2,
        &first.mode.id,
        "no frame in 2000 ms",
    );
    let next = plan_frames_with(&dev, &req, &registry()).unwrap();
    assert_eq!(next.mode.format.code, FourCc::MJPG, "{next}");
    assert_eq!(next.mode.format.resolution.width.get(), 640);
}

/// Fallback order when the bus refuses uncompressed modes: the largest YUYV first, then the MJPG
/// mode, and only then the smaller YUYV. Each mode is started once, and the one that streams
/// ends the fallback.
#[test]
fn enospc_fallback_tries_mjpg_before_smaller_yuyv() {
    use crate::capture_api::CaptureError;
    let req = Frames::any().fps(30);
    let dev = camera_with(
        "enospc-order",
        "480",
        vec![
            mode(FourCc::YUYV, 2560, 1440, 30),
            mode(FourCc::MJPG, 640, 480, 30),
            mode(FourCc::YUYV, 320, 240, 30),
        ],
    );
    let attempts = std::cell::RefCell::new(Vec::new());
    let (code, width, _) = stalled::open_with_fallback(
        || plan_frames_with(&dev, &req, &registry()),
        |plan| {
            let res = plan.mode.format.resolution;
            attempts
                .borrow_mut()
                .push((plan.mode.format.code, res.width.get()));
            // The bus refuses the 2560 and the MJPG 640; the smallest YUYV streams.
            if res.width.get() >= 640 {
                return Err(stalled::start_failed(
                    &dev,
                    BackendKind::V4l2,
                    &plan.mode,
                    CaptureError::Backend("uvc: No space left on device (os error 28)".into()),
                ));
            }
            Ok((plan.mode.format.code, res.width.get(), res.height.get()))
        },
    )
    .expect("the smallest YUYV streams");
    assert_eq!((code, width), (FourCc::YUYV, 320));
    assert_eq!(
        attempts.into_inner(),
        vec![
            (FourCc::YUYV, 2560),
            (FourCc::MJPG, 640),
            (FourCc::YUYV, 320)
        ],
        "one start per mode, MJPG before the smaller YUYV"
    );
}

/// No-frames stalls are not limited to one retry: each stalled mode is left out and the next
/// one is tried, until a mode delivers.
#[test]
fn stalls_keep_replanning_until_a_mode_delivers() {
    use crate::capture_api::CaptureError;
    let req = Frames::any().fps(30);
    let dev = camera_with(
        "stall-keeps-replanning",
        "480",
        vec![
            mode(FourCc::YUYV, 2560, 1440, 30),
            mode(FourCc::MJPG, 640, 480, 30),
            mode(FourCc::YUYV, 320, 240, 30),
        ],
    );
    let attempts = std::cell::Cell::new(0);
    let width = stalled::open_with_fallback(
        || plan_frames_with(&dev, &req, &registry()),
        |plan| {
            attempts.set(attempts.get() + 1);
            let res = plan.mode.format.resolution;
            if res.width.get() > 320 {
                // What confirm_first_frame records for a mode that delivers nothing.
                stalled::mark(
                    &dev,
                    BackendKind::V4l2,
                    &plan.mode.id,
                    "no frame in 2000 ms",
                );
                return Err(CaptureError::NoFrames("no frame in 2000 ms".into()));
            }
            Ok(res.width.get())
        },
    )
    .expect("the third mode delivers");
    assert_eq!(width, 320);
    assert_eq!(attempts.get(), 3);
}

/// A bus that refuses an uncompressed mode refuses every uncompressed mode carrying at least as
/// many bytes per second, without a start for each: here YUYV 1920x1080 (124 MB/s) after the
/// refused YUYV 1280x720 (28 MB/s). A lighter mode (YUYV 640x480, 18 MB/s) is still tried.
#[test]
fn a_refused_stream_excludes_heavier_uncompressed_modes() {
    use crate::capture_api::CaptureError;
    let mut dev = camera_with(
        "rate-class",
        "480",
        vec![
            mode(FourCc::YUYV, 1920, 1080, 30),
            mode(FourCc::YUYV, 1280, 720, 30),
            mode(FourCc::YUYV, 640, 480, 30),
            mode(FourCc::MJPG, 320, 240, 30),
        ],
    );
    dev.identity.keys = vec!["test:rate-class".into()];
    let req = Frames::any().fps(30);
    let refused = dev.backends[0]
        .descriptor
        .modes
        .iter()
        .find(|m| m.format.code == FourCc::YUYV && m.format.resolution.width.get() == 1280)
        .cloned()
        .expect("the 1280x720 mode");
    let _ = stalled::start_failed(
        &dev,
        BackendKind::V4l2,
        &refused,
        CaptureError::Backend("uvc: No space left on device (os error 28)".into()),
    );
    let plan = plan_frames_with(&dev, &req, &registry()).unwrap();
    assert_eq!(
        (
            plan.mode.format.code,
            plan.mode.format.resolution.width.get()
        ),
        (FourCc::YUYV, 640),
        "{plan}"
    );
    let heavier = plan
        .rejected
        .iter()
        .find(|r| r.candidate == "v4l2 YUYV 1920x1080")
        .map(|r| r.reason.as_str());
    assert!(
        heavier.is_some_and(|r| r.contains("not enough USB bandwidth")),
        "{heavier:?}"
    );
}
