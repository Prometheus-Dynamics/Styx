use super::*;
use crate::planner::{Hardware, Unmet};

#[test]
fn requests_survive_the_wire() {
    use crate::BackendKind;
    use crate::planner::Frames;

    let req = Frames::formats([FourCc::NV12, FourCc::MJPG])
        .size(320, 180)
        .size_at_most(1920, 1080)
        .fps_between(15, 60)
        .every_frame(3)
        .pyramid(2)
        .regions([
            FrameRect::new(1, 2, 3, 4),
            FrameRect::new(10, 20, 30, 40),
            FrameRect::new(5, 6, 7, 8),
        ])
        .skip_stale_regions()
        .overview(320, 200)
        .row_alignment(64)
        .backend(BackendKind::Libcamera)
        .hardware(Hardware::Off)
        .forbid("ffmpeg")
        .decode_threads(2)
        .strict();
    let ClientMessage::Request(back, camera, priority, _) = decode_client(&encode_request(
        &req,
        Some("ov9782"),
        crate::ipc::ClientPriority::Normal,
    ))
    .unwrap() else {
        panic!("not a request");
    };
    assert_eq!(*back, req);
    assert_eq!(camera.as_deref(), Some("ov9782"));
    assert_eq!(priority, crate::ipc::ClientPriority::Normal);
    // A low-priority request: the same request, and the priority as a trailer older services
    // stop reading before (the request bytes are a prefix).
    let low = encode_request(&req, Some("ov9782"), crate::ipc::ClientPriority::Low);
    let normal = encode_request(&req, Some("ov9782"), crate::ipc::ClientPriority::Normal);
    assert!(low.starts_with(&normal) && low.len() == normal.len() + 2);
    let ClientMessage::Request(back, _, priority, _) = decode_client(&low).unwrap() else {
        panic!("not a request");
    };
    assert_eq!(*back, req);
    assert_eq!(priority, crate::ipc::ClientPriority::Low);
    for req in [
        Frames::gray().fps(30),
        Frames::any().fps_at_least(15),
        Frames::rgb(),
    ] {
        let ClientMessage::Request(back, None, _, _) = decode_client(&encode_request(
            &req,
            None,
            crate::ipc::ClientPriority::Normal,
        ))
        .unwrap() else {
            panic!("not a request");
        };
        assert_eq!(*back, req);
    }
    let cameras = vec![CameraInfo {
        name: "ov9782".into(),
        keys: vec!["i2c:ov9782".into()],
        in_use: true,
        modes: Vec::new(),
    }];
    let ServerMessage::Cameras(back) = decode_server(&encode_cameras(&cameras)).unwrap() else {
        panic!("not a camera list");
    };
    assert_eq!(back, cameras);
    let delivered = Delivered {
        format: FourCc::GREY,
        size: (1280, 800),
        fps: Some(59.94),
        pyramid_levels: 2,
        hardware_pyramid_level: Some(1),
        inter_coded: false,
        roi: Some(crate::planner::RoiCrop::Isp),
        regions: vec![
            Some(crate::planner::RoiCrop::Isp),
            Some(crate::planner::RoiCrop::IspPass),
            None,
            Some(crate::planner::RoiCrop::View),
        ],
        overview: Some((320, 200)),
        hardware_overview: true,
        unmet: vec![
            Unmet::Size {
                wanted: (160, 90),
                delivered: (1280, 800),
            },
            Unmet::Roi,
            Unmet::Overview {
                wanted: (100, 100),
                delivered: (640, 400),
            },
        ],
    };
    let ServerMessage::Accept(plan, back, None) =
        decode_server(&encode_accept("the plan", &delivered, None)).unwrap()
    else {
        panic!("not an accept");
    };
    assert_eq!((plan.as_str(), &*back), ("the plan", &delivered));
    // The client's token is a trailer: an older client stops before it.
    let token = ClientToken { id: 4, token: 77 };
    let with_token = encode_accept("the plan", &delivered, Some(token));
    let ServerMessage::Accept(_, back, Some(got)) = decode_server(&with_token).unwrap() else {
        panic!("no token");
    };
    assert_eq!((*back, got), (delivered, token));
    let request = encode_metrics_request(MetricsFormat::Prometheus);
    assert!(matches!(
        decode_client(&request),
        Ok(ClientMessage::Metrics(MetricsFormat::Prometheus))
    ));
    assert!(matches!(
        decode_server(&encode_metrics_reply(MetricsFormat::Json, 1234)),
        Ok(ServerMessage::Metrics(MetricsFormat::Json, 1234))
    ));
}

#[test]
fn sliced_frame_trailers_survive_the_wire_and_older_readers_skip_them() {
    use crate::ipc::ClientPriority;
    use crate::planner::Frames;

    // The request: the slices flag after the priority trailer, alone or with it.
    let req = Frames::gray().size(1280, 800).pyramid(1);
    for priority in [ClientPriority::Normal, ClientPriority::Low] {
        let plain = encode_request(&req, None, priority);
        let sliced = encode_request_with(&req, None, priority, true);
        assert!(sliced.starts_with(&plain) && sliced.len() == plain.len() + 2);
        let ClientMessage::Request(back, _, p, slices) = decode_client(&sliced).unwrap() else {
            panic!("not a request");
        };
        assert_eq!((*back == req, p, slices), (true, priority, true));
        let ClientMessage::Request(_, _, _, slices) = decode_client(&plain).unwrap() else {
            panic!("not a request");
        };
        assert!(!slices);
    }
    // The frame: the progress trailer after the hops one, with or without hops.
    let progress = WireProgress {
        slot: 5,
        heights: [800, 400],
        ring_attached: true,
        outputs: smallvec::smallvec![0, 1],
    };
    let mut frame = WireFrame::empty();
    frame.layouts.push(PlaneLayout {
        offset: 0,
        len: 16,
        stride: 4,
    });
    frame.backing = WireBacking::Memfd { len: 16 };
    let mut hops = FrameHops::new();
    hops.set(Hop::Sensor, 1_000);
    for hops in [hops, FrameHops::new()] {
        let mut bytes = Vec::new();
        encode_frame_with_progress(9, &frame, &hops, Some(&progress), &mut bytes);
        let (mut back, mut got) = (WireFrame::empty(), None);
        assert_eq!(
            decode_frame_with_progress(&bytes, &mut back, &mut got).unwrap(),
            Some(9)
        );
        assert_eq!(got.as_ref(), Some(&progress));
        assert_eq!(back.meta.hops.get(Hop::Sensor), hops.get(Hop::Sensor));
        // A reader that takes no progress still reads the frame.
        let mut plain = WireFrame::empty();
        assert_eq!(decode_frame_into(&bytes, &mut plain).unwrap(), Some(9));
        assert_eq!(plain.layouts, frame.layouts);
    }
}

#[test]
fn native_metadata_and_its_3a_record_cross_the_wire() {
    let aaa = NativeAaa {
        colour_gains: [1.9, 1.0, 1.6],
        colour_temperature_k: 3665.0,
        lux: 210.5,
        ae_locked: true,
        awb_converged: true,
        flicker_period_ns: 10_000_000,
        lens: Some(NativeLens {
            position_dioptres: 2.5,
            af_state: NativeAfState::Focused,
            settled: Some(true),
        }),
    };
    let native = NativeFrameMeta {
        sequence: 77,
        exposure_ns: 8_100_000,
        analog_gain: 2.0,
        digital_gain: 1.25,
        frame_duration_ns: 8_333_333,
        verified: true,
        frame_end_ns: 123_456_789,
        aaa: Some(aaa),
        ..Default::default()
    };
    let mut frame = WireFrame::empty();
    frame.layouts.push(PlaneLayout {
        offset: 0,
        len: 16,
        stride: 4,
    });
    frame.backing = WireBacking::Memfd { len: 16 };
    let progress = WireProgress {
        slot: 1,
        heights: [800, 400],
        ring_attached: false,
        outputs: smallvec::smallvec![0],
    };
    for (lens, progress) in [(true, Some(&progress)), (false, None)] {
        let mut n = native;
        if !lens {
            n.aaa = Some(NativeAaa { lens: None, ..aaa });
        }
        frame.meta.backend = Some(BackendFrameMeta::Native(n));
        let mut bytes = Vec::new();
        encode_frame_with_progress(3, &frame, &FrameHops::new(), progress, &mut bytes);
        let (mut back, mut got) = (WireFrame::empty(), None);
        decode_frame_with_progress(&bytes, &mut back, &mut got).unwrap();
        assert_eq!(back.meta.backend, Some(BackendFrameMeta::Native(n)));
        assert_eq!(got.is_some(), progress.is_some());
        // A frame without the trailer (another backend, an older service) has no native meta,
        // also when the frame it is decoded into had some.
        frame.meta.backend = None;
        encode_frame_with_progress(4, &frame, &FrameHops::new(), progress, &mut bytes);
        decode_frame_with_progress(&bytes, &mut back, &mut got).unwrap();
        assert_eq!(back.meta.backend, None);
    }
}

#[test]
fn effective_ranges_follow_the_control_list_and_older_readers_stop_before_them() {
    use super::controls::{decode_control_list, encode_control_list};
    use crate::ipc::ControlDescriptor;
    let meta = |id, name: &str, max| ControlMeta {
        id: ControlId(id),
        name: name.into(),
        kind: ControlKind::Uint,
        access: Access::ReadWrite,
        min: ControlValue::Uint(1),
        max: ControlValue::Uint(max),
        default: ControlValue::Uint(1),
        step: None,
        menu: None,
        metadata: ControlMetadata::default(),
    };
    let plain = ControlDescriptor {
        meta: meta(1, "plain", 10),
        current: None,
        standard: None,
        writable: true,
        effective_min: None,
        effective_max: None,
    };
    let exposure = ControlDescriptor {
        meta: meta(2, "exposure_time_us", 476_066),
        effective_min: Some(ControlValue::Uint(14)),
        effective_max: Some(ControlValue::Uint(8_142)),
        ..plain.clone()
    };
    let list = vec![plain.clone(), exposure];
    let ranges = |l: &[ControlDescriptor]| -> Vec<_> {
        l.iter()
            .map(|d| (d.meta.id, d.effective_min.clone(), d.effective_max.clone()))
            .collect()
    };
    let bytes = encode_control_list(&list);
    assert_eq!(ranges(&decode_control_list(&bytes).unwrap()), ranges(&list));
    // Without ranges nothing follows the list (as older services sent it).
    let only = encode_control_list(std::slice::from_ref(&plain));
    assert_eq!(
        ranges(&decode_control_list(&only).unwrap()),
        ranges(&[plain])
    );
}

#[test]
fn a_sensor_mode_crosses_the_wire_after_the_other_trailers() {
    use crate::ipc::ClientPriority;
    use crate::planner::Frames;
    let req = Frames::gray().size(320, 200).sensor_mode(1280, 800);
    for (priority, slices) in [(ClientPriority::Normal, false), (ClientPriority::Low, true)] {
        let bytes = encode_request_with(&req, None, priority, slices);
        let ClientMessage::Request(back, _, p, s) = decode_client(&bytes).unwrap() else {
            panic!("not a request");
        };
        assert_eq!(
            (back.sensor_mode, p, s),
            (Some((1280, 800)), priority, slices)
        );
    }
    let plain = encode_request(&Frames::gray(), None, ClientPriority::Normal);
    let ClientMessage::Request(back, ..) = decode_client(&plain).unwrap() else {
        panic!("not a request");
    };
    assert_eq!(back.sensor_mode, None);
}
