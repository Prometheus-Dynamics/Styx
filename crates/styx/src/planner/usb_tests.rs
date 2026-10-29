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
            &plan.mode.id,
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
                &plan.mode.id,
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
