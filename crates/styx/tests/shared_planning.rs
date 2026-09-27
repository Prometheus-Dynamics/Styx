//! Several consumers of one camera share one capture: each gets its own prepared frames, all
//! from the same captured frames. C270 MJPEG frames are recorded, replayed as a camera, planned
//! for a small luma detector and a full-size RGB consumer, and delivered.

#![cfg(all(feature = "codec-turbojpeg", feature = "replay-mcap"))]

use std::path::PathBuf;
use std::time::Duration;

use styx::DeviceIdentity;
use styx::planner::plan_many;
use styx::prelude::*;

const FIXTURE: &[u8] = include_bytes!("../../../testing/fixtures/c270_720p_rst.mjpeg");

/// The C270 fixture as a replayable MJPEG 1280x720 camera.
fn c270_recording(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("styx-{name}-{}.mcap", std::process::id()));
    let res = Resolution::new(1280, 720).unwrap();
    let format = MediaFormat::new(FourCc::MJPG, res, ColorSpace::Srgb);
    let header = RecordingHeader {
        device: DeviceIdentity {
            display: "c270 fixture".into(),
            keys: vec!["fixture:c270".into()],
        },
        backend: "v4l2".into(),
        format,
        interval: Interval::from_fps(30),
    };
    let mut recorder = StreamRecorder::with_header(&path, &header).unwrap();
    let starts: Vec<usize> = (0..FIXTURE.len() - 2)
        .filter(|&i| FIXTURE[i..i + 3] == [0xFF, 0xD8, 0xFF])
        .collect();
    for (k, &start) in starts.iter().enumerate() {
        let jpeg = &FIXTURE[start..*starts.get(k + 1).unwrap_or(&FIXTURE.len())];
        let mut buf = BufferPool::with_limits(1, jpeg.len(), 0).lease();
        buf.resize(jpeg.len());
        buf.as_mut_slice().copy_from_slice(jpeg);
        let meta = FrameMeta::new(format, k as u64 * 33_000_000);
        recorder
            .record(&FrameLease::single_plane(meta, buf, jpeg.len(), jpeg.len()))
            .unwrap();
    }
    recorder.finish().unwrap();
    path
}

fn frame(frames: &mut styx::planner::PlannedFrames) -> FrameLease {
    for _ in 0..50 {
        if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(200)) {
            return frame;
        }
    }
    panic!("no frame");
}

#[test]
fn consumers_of_one_camera_get_their_own_frames_from_one_capture() {
    let path = c270_recording("shared");
    let source = CaptureRequest::replay_source(
        ReplaySourceConfig::new(&path)
            .pacing(ReplayPacing::Realtime)
            .loop_forever(true),
    )
    .unwrap();
    let plan = plan_many(
        source.device(),
        &[
            FrameRequirements::luma().output_resolution(320, 180),
            FrameRequirements::formats([FourCc::RG24]),
        ],
    )
    .unwrap();
    assert_eq!(plan.consumers[0].output_resolution(), (320, 180));
    assert_eq!(plan.consumers[1].output_resolution(), (1280, 720));
    let mut consumers = plan.start().unwrap();
    let mut rgb = consumers.pop().unwrap();
    let mut luma = consumers.pop().unwrap();

    let small = frame(&mut luma);
    assert_eq!(small.meta().format.code, FourCc::GREY);
    assert_eq!(small.meta().format.resolution.width.get(), 320);
    // The frame the detector pulled was shared with the RGB consumer, prepared its own way
    // (the next frame is still ~33 ms away).
    let full = frame(&mut rgb);
    assert_eq!(full.meta().format.code, FourCc::RG24);
    assert_eq!(full.meta().format.resolution.width.get(), 1280);
    assert_eq!(full.meta().timestamp, small.meta().timestamp);

    // A consumer that falls behind gets the newest frame, not a backlog.
    let latest = (0..5)
        .map(|_| frame(&mut luma).meta().timestamp)
        .last()
        .unwrap();
    assert_eq!(frame(&mut rgb).meta().timestamp, latest);
    assert!(rgb.health_report().drop_count >= 4);

    // One consumer leaving does not stop the other.
    drop(luma);
    for _ in 0..5 {
        assert_eq!(frame(&mut rgb).meta().format.code, FourCc::RG24);
    }
    drop(rgb);
    let _ = std::fs::remove_file(path);
}

#[test]
fn consumers_with_the_same_needs_share_one_decode() {
    let path = c270_recording("shared-decode");
    let source = CaptureRequest::replay_source(
        ReplaySourceConfig::new(&path)
            .pacing(ReplayPacing::Realtime)
            .loop_forever(true),
    )
    .unwrap();
    let detector = FrameRequirements::luma().output_resolution(320, 180);
    // The same needs, but only the top-left quarter of the frame.
    let region = detector.clone().roi(FrameRect::new(0, 0, 640, 360));
    let plan = plan_many(
        source.device(),
        &[detector, FrameRequirements::formats([FourCc::RG24]), region],
    )
    .unwrap();
    assert!(
        plan.consumers[0]
            .notes
            .iter()
            .any(|n| n.contains("prepared once for consumers 0, 2")),
        "{plan}"
    );
    assert!(
        !plan.consumers[1]
            .notes
            .iter()
            .any(|n| n.contains("prepared once"))
    );
    let mut consumers = plan.start().unwrap();
    let mut cropped = consumers.pop().unwrap();
    let _rgb = consumers.pop().unwrap();
    let mut whole = consumers.pop().unwrap();

    let a = frame(&mut whole);
    let b = frame(&mut cropped);
    assert_eq!(a.meta().timestamp, b.meta().timestamp);
    assert_eq!(a.meta().format.resolution.width.get(), 320);
    // The region is a view of the same decoded pixels (a 1/4 decode of 1280x720).
    assert_eq!(b.meta().format.resolution.width.get(), 160);
    let (a_rows, b_rows) = (a.luma_rows().unwrap(), b.luma_rows().unwrap());
    assert_eq!(
        a_rows.row(0).unwrap().data().as_ptr(),
        b_rows.row(0).unwrap().data().as_ptr()
    );
    assert_eq!(
        b_rows.row(5).unwrap().data(),
        &a_rows.row(5).unwrap().data()[..160]
    );
    drop((whole, cropped));
    let _ = std::fs::remove_file(path);
}
