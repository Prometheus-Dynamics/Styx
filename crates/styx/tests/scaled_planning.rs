//! Plans that ask for a smaller output decode real camera MJPEG at that size, end to end:
//! C270 frames are recorded, replayed as a camera, planned and delivered.

#![cfg(all(feature = "codec-turbojpeg", feature = "replay-mcap"))]

use std::path::PathBuf;
use std::time::Duration;

use styx::DeviceIdentity;
use styx::planner::plan_frames;
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

fn first_frame(requirements: &FrameRequest, name: &str) -> (FrameLease, (u32, u32)) {
    let path = c270_recording(name);
    let source =
        CaptureRequest::replay_source(ReplaySourceConfig::new(&path).pacing(ReplayPacing::Unpaced))
            .unwrap();
    let plan = plan_frames(source.device(), requirements).unwrap();
    let planned_size = plan.output_resolution();
    let mut frames = plan.start().unwrap();
    let frame = loop {
        match frames.next_frame(Duration::from_secs(5)) {
            RecvOutcome::Data(frame) => break frame,
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => panic!("replay closed before a frame"),
        }
    };
    frames.stop();
    let _ = std::fs::remove_file(path);
    (frame, planned_size)
}

#[test]
fn luma_is_decoded_at_the_requested_size() {
    let (frame, planned) = first_frame(&Frames::gray().size(320, 180).pyramid(1), "scaled-luma");
    assert_eq!(planned, (320, 180));
    let res = frame.meta().format.resolution;
    assert_eq!((res.width.get(), res.height.get()), (320, 180));
    assert_eq!(frame.meta().format.code, FourCc::GREY);
    let level = frame
        .pyramid_level(1)
        .expect("pyramid level from the scaled frame");
    assert_eq!(level.meta().format.resolution.width.get(), 160);
}

#[test]
fn roi_is_given_in_capture_pixels_and_mapped_to_the_scaled_frame() {
    // The right half of the 1280x720 capture is the right half of the 640x360 output.
    let (frame, _) = first_frame(
        &Frames::gray()
            .size(640, 360)
            .roi(FrameRect::new(640, 0, 640, 720)),
        "scaled-roi",
    );
    let crop = frame.meta().crop.expect("decoded region");
    assert!(crop.x <= 320 && crop.x + crop.width >= 640, "{crop:?}");
    assert_eq!(crop.height, 360);
}

#[test]
fn rgb_is_decoded_at_the_requested_size() {
    let (frame, planned) = first_frame(
        &Frames::formats([FourCc::RG24]).size(640, 360),
        "scaled-rgb",
    );
    assert_eq!(planned, (640, 360));
    let res = frame.meta().format.resolution;
    assert_eq!((res.width.get(), res.height.get()), (640, 360));
    assert_eq!(frame.planes()[0].data().len(), 640 * 360 * 3);
}
