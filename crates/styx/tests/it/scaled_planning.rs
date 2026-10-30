//! Plans that ask for a smaller output decode real camera MJPEG at that size, end to end:
//! C270 frames are recorded, replayed as a camera, planned and delivered.

use std::time::Duration;

use styx::planner::plan_frames;
use styx::prelude::*;

use crate::support::c270::c270_recording;

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
