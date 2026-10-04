//! Regions of interest and overviews on a camera without an ISP to crop: luma frames are views
//! of the region, the overview is the uncropped frame, and the region moves while running.

#![cfg(feature = "facade")]

use std::time::Duration;

use styx::planner::RoiCrop;
use styx::prelude::*;

fn camera() -> ProbedDevice {
    CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .name("regions")
            .format(FourCc::NV12)
            .resolution(64, 48)
            .fps(100),
    )
    .into_device()
}

fn size(frame: &FrameLease) -> (u32, u32) {
    let r = frame.meta().format.resolution;
    (r.width.get(), r.height.get())
}

fn next(frames: &mut Frames) -> FrameLease {
    match frames.next_frame(Duration::from_secs(2)) {
        RecvOutcome::Data(frame) => frame,
        RecvOutcome::Empty => panic!("no frame"),
        RecvOutcome::Closed => panic!("frames closed"),
    }
}

#[test]
fn views_of_the_region_carry_the_whole_frame_as_their_overview() {
    let region = FrameRect::new(8, 4, 16, 12);
    let mut frames = Frames::gray()
        .roi(region)
        .overview(16, 12)
        .open(&camera())
        .unwrap();
    let delivered = frames.plan().delivered();
    assert_eq!(delivered.roi, Some(RoiCrop::View));
    assert_eq!(
        (delivered.overview, delivered.hardware_overview),
        (Some((64, 48)), false)
    );
    let frame = next(&mut frames);
    assert_eq!(size(&frame), (16, 12));
    assert_eq!(frame.meta().crop, Some(region));
    let overview = frame.overview().expect("overview");
    assert_eq!(size(overview), (64, 48));
    assert_eq!(overview.meta().crop, None);
    assert_eq!(overview.meta().format.code, FourCc::GREY);
    // The region moves; the overview stays whole.
    let moved = FrameRect::new(32, 16, 16, 16);
    frames.roi().set(Some(moved));
    let frame = next(&mut frames);
    assert_eq!(frame.meta().crop, Some(moved));
    assert_eq!(size(frame.overview().expect("overview")), (64, 48));
    frames.roi().set(None);
    let frame = next(&mut frames);
    assert_eq!((size(&frame), frame.meta().crop), ((64, 48), None));
}
