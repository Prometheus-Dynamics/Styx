//! Consumers that want compressed frames the camera does not produce: the planner adds an
//! encoder, consumers with the same needs share one stream, and each one starts, and resumes
//! after a gap, at a keyframe it asked for.

use std::time::Duration;

use styx::planner::{plan_frames, plan_many};
use styx::prelude::*;

fn camera() -> ProbedDevice {
    VirtualSourceConfig::new()
        .name("virtual-encode")
        .format(FourCc::RG24)
        .resolution(640, 360)
        .fps(60)
        .into_device()
}

fn packet(frames: &mut styx::planner::Frames) -> FrameLease {
    for _ in 0..100 {
        if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(100)) {
            return frame;
        }
    }
    panic!("no packet");
}

/// Whether an Annex B H.264 packet carries a sequence parameter set (a stream can start here).
fn has_sps(packet: &FrameLease) -> bool {
    let data = packet.planes()[0].data();
    data.windows(4)
        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
}

#[test]
fn consumers_get_an_encoded_stream_they_can_start_on() {
    let h264 = Frames::formats([FourCc::H264]);
    let plan = plan_frames(&camera(), &h264).unwrap();
    let encode = plan
        .steps
        .iter()
        .find(|s| s.kind == styx::planner::StepKind::Encode)
        .unwrap_or_else(|| panic!("no encode step: {plan}"));
    assert!(encode.detail.contains("H264"), "{plan}");
    assert!(plan.inter_coded());

    // Two viewers and a raw consumer on one capture; the viewers share one encoder.
    let shared = plan_many(
        &camera(),
        &[h264.clone(), Frames::formats([FourCc::RG24]), h264],
    )
    .unwrap();
    assert!(
        shared.consumers[0]
            .notes
            .iter()
            .any(|n| n.contains("prepared once for consumers 0, 2")),
        "{shared}"
    );
    let mut consumers = shared.start().unwrap();
    let mut late = consumers.pop().unwrap();
    let _raw = consumers.pop().unwrap();
    let mut viewer = consumers.pop().unwrap();

    let first = packet(&mut viewer);
    assert_eq!(first.meta().format.code, FourCc::H264);
    assert!(!first.meta().delta && has_sps(&first));
    // The viewer's stream goes on with inter-coded packets.
    let deltas = (0..20).filter(|_| packet(&mut viewer).meta().delta).count();
    assert!(deltas > 10, "{deltas} delta packets of 20");

    // The other viewer took nothing yet (its queue dropped packets): its next packet is a
    // keyframe with the stream headers, made for it.
    let resumed = packet(&mut late);
    assert!(!resumed.meta().delta && has_sps(&resumed));
    // And the first viewer's stream is unaffected apart from that keyframe.
    assert_eq!(packet(&mut viewer).meta().format.code, FourCc::H264);
}

#[test]
fn a_viewer_joining_a_served_stream_starts_at_a_keyframe() {
    use styx::ipc::{CameraService, FrameClient};

    let socket = std::env::temp_dir().join(format!("styx-h264-{}.sock", std::process::id()));
    let service = CameraService::new(camera())
        .keep_streaming()
        .serve(&socket)
        .unwrap();
    let h264 = Frames::formats([FourCc::H264]);
    let recv = |client: &FrameClient| {
        for _ in 0..100 {
            if let RecvOutcome::Data(frame) = client.recv(Duration::from_millis(100)) {
                return frame;
            }
        }
        panic!("no packet");
    };
    let first = FrameClient::request(&socket, &h264).unwrap();
    assert!(first.plan().unwrap().contains("encode"));
    for _ in 0..10 {
        recv(&first);
    }
    let second = FrameClient::request(&socket, &h264).unwrap();
    let start = recv(&second);
    assert!(!start.meta().delta && has_sps(&start));
    assert_eq!(service.stats().restarts, 0);
}
