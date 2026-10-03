use super::*;
use styx_kernel::usbfs::Speed;
use styx_uvc::sysfs::InterfaceBinding;

/// The C270 as sysfs shows it on the CM5, `uvcvideo` bound.
fn c270() -> UsbCameraInfo {
    let function =
        styx_uvc::UvcFunction::parse(include_bytes!("../../../uvc/tests/data/c270.bin")).unwrap();
    let bound = |number| InterfaceBinding {
        number,
        driver: Some("uvcvideo".into()),
    };
    UsbCameraInfo {
        sysfs: "/sys/bus/usb/devices/3-1".into(),
        port: "3-1".into(),
        busnum: 3,
        devnum: 2,
        vendor_id: 0x046d,
        product_id: 0x0825,
        manufacturer: None,
        product: None,
        serial: Some("01A989E0".into()),
        speed: Speed::High,
        bus_info: "usb-xhci-hcd.1-1".into(),
        function,
        interfaces: vec![bound(0), bound(1)],
    }
}

#[test]
fn backend_kind_parses() {
    assert_eq!("uvc".parse::<BackendKind>(), Ok(BackendKind::Uvc));
    assert_eq!("usbfs".parse::<BackendKind>(), Ok(BackendKind::Uvc));
    assert_eq!(BackendKind::Uvc.to_string(), "uvc");
}

#[test]
fn modes_match_what_uvcvideo_reports() {
    let modes = modes(&c270());
    assert_eq!(modes.len(), 38);
    let vga = &modes[0];
    assert_eq!(vga.format.code, FourCc::new(*b"YUYV"));
    assert_eq!(vga.format.resolution.width.get(), 640);
    // 1/30, 1/25, 1/20, 1/15, 1/10, 1/5 s, as VIDIOC_ENUM_FRAMEINTERVALS gives them.
    let fps: Vec<u32> = vga.intervals.iter().map(|i| i.denominator.get()).collect();
    assert_eq!(fps, vec![30, 25, 20, 15, 10, 5]);
    assert!(vga.intervals.iter().all(|i| i.numerator.get() == 1));
    let hd = modes
        .iter()
        .find(|m| m.format.code == FourCc::new(*b"MJPG") && m.format.resolution.width.get() == 1280)
        .unwrap();
    assert_eq!(hd.intervals[0], Interval::from_fps(30).unwrap());
    assert_eq!(interval_100ns(Interval::from_fps(30).unwrap()), 333_333);
}

#[test]
fn plane_layouts() {
    let yuyv = layouts(FourCc::new(*b"YUYV"), 640, 480, 614_400).unwrap();
    assert_eq!((yuyv[0].len, yuyv[0].stride), (614_400, 1280));
    // A short frame has no layout.
    assert!(layouts(FourCc::new(*b"YUYV"), 640, 480, 614_396).is_none());
    let nv12 = layouts(FourCc::new(*b"NV12"), 64, 32, 64 * 48).unwrap();
    assert_eq!((nv12[1].offset, nv12[1].len), (2048, 1024));
    let mjpg = layouts(FourCc::new(*b"MJPG"), 1280, 720, 24_402).unwrap();
    assert_eq!(mjpg[0].len, 24_402);
}

fn v4l2_device() -> ProbedDevice {
    ProbedDevice {
        identity: DeviceIdentity {
            display: "UVC Camera (046d:0825)".into(),
            keys: vec!["usb-xhci-hcd.1-1".into(), "046d:0825".into()],
        },
        backends: vec![ProbedBackend {
            kind: BackendKind::V4l2,
            handle: BackendHandle::Virtual,
            descriptor: CaptureDescriptor::new(modes(&c270())),
            properties: Vec::new(),
        }],
    }
}

/// Merged into uvcvideo's device after it: V4L2 stays the default backend and the planner
/// only plans the userspace one when asked for it, or when nothing else has the camera.
#[test]
fn uvcvideo_stays_the_default() {
    let info = c270();
    let backend = backend_for(&info, Vec::new());
    let mut devices = vec![v4l2_device()];
    merge(&mut devices, &info, backend.clone());
    assert_eq!(devices.len(), 1);
    let d = &devices[0];
    assert_eq!(d.backends[1].kind, BackendKind::Uvc);
    assert_eq!(d.default_backend().unwrap().kind, BackendKind::V4l2);
    assert!(d.identity.keys.contains(&"usb:3-1".to_string()));

    let yuyv = FourCc::new(*b"YUYV");
    let plan = crate::planner::plan_frames(d, &crate::planner::Frames::formats([yuyv])).unwrap();
    assert_eq!(plan.backend, BackendKind::V4l2);
    let asked = crate::planner::Frames::formats([yuyv]).backend(BackendKind::Uvc);
    let plan = crate::planner::plan_frames(d, &asked).unwrap();
    assert_eq!(plan.backend, BackendKind::Uvc);

    let mut alone = Vec::new();
    merge(&mut alone, &info, backend);
    assert_eq!(alone[0].identity.display, "UVC Camera (046d:0825)");
    let plan =
        crate::planner::plan_frames(&alone[0], &crate::planner::Frames::formats([yuyv])).unwrap();
    assert_eq!(plan.backend, BackendKind::Uvc);
}
