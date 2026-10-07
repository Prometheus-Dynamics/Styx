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
    let ClientMessage::Request(back, camera, priority) = decode_client(&encode_request(
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
    let ClientMessage::Request(back, _, priority) = decode_client(&low).unwrap() else {
        panic!("not a request");
    };
    assert_eq!(*back, req);
    assert_eq!(priority, crate::ipc::ClientPriority::Low);
    for req in [
        Frames::gray().fps(30),
        Frames::any().fps_at_least(15),
        Frames::rgb(),
    ] {
        let ClientMessage::Request(back, None, _) = decode_client(&encode_request(
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
