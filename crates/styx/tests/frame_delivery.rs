//! Delivery is its own choice: `latest()` hands out the newest frame only, `every_frame(n)`
//! queues up to `n` frames for a busy consumer and counts the ones it still has to drop.

#![cfg(feature = "facade")]

use std::thread::sleep;
use std::time::Duration;

use styx::prelude::*;

fn camera(name: &str) -> ProbedDevice {
    CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .name(name)
            .resolution(64, 48)
            .fps(100),
    )
    .into_device()
}

/// Frames waiting now, without waiting for more.
fn waiting(frames: &mut Frames) -> Vec<FrameLease> {
    let mut out = Vec::new();
    while let RecvOutcome::Data(frame) = frames.next_frame(Duration::ZERO) {
        out.push(frame);
        if out.len() > 64 {
            break;
        }
    }
    out
}

#[test]
fn latest_keeps_only_the_newest_frame() {
    let mut frames = Frames::any().open(&camera("latest")).unwrap();
    assert_eq!(frames.plan().queue_depth(), 1);
    // The first frame, then a consumer busy for 15 frame periods.
    assert!(matches!(
        frames.next_frame(Duration::from_secs(2)),
        RecvOutcome::Data(_)
    ));
    sleep(Duration::from_millis(150));
    let got = waiting(&mut frames);
    assert!(got.len() <= 2, "{} frames waited", got.len());
    assert!(frames.dropped() > 0, "dropped {}", frames.dropped());
}

#[test]
fn every_frame_queues_frames_in_order_and_counts_drops() {
    let mut frames = Frames::any().every_frame(8).open(&camera("every")).unwrap();
    assert_eq!(frames.plan().queue_depth(), 8);
    assert!(matches!(
        frames.next_frame(Duration::from_secs(2)),
        RecvOutcome::Data(_)
    ));
    // Busy for about 6 frame periods: those frames wait, none lost.
    sleep(Duration::from_millis(60));
    let got = waiting(&mut frames);
    assert!(got.len() >= 2, "{} frames waited", got.len());
    let times: Vec<u64> = got.iter().map(|f| f.meta().timestamp).collect();
    assert!(times.windows(2).all(|w| w[0] < w[1]), "{times:?}");
    // Busy for 30 frame periods: the queue holds 8, the rest are counted.
    sleep(Duration::from_millis(300));
    let got = waiting(&mut frames);
    assert!((6..=10).contains(&got.len()), "{} frames waited", got.len());
    assert!(frames.dropped() > 0, "dropped {}", frames.dropped());
}

#[test]
fn camera_first_requests_open_the_same_frames() {
    let device = camera("camera-first");
    let mut frames = device.frames().every_frame(2).open().unwrap();
    assert_eq!(frames.plan().queue_depth(), 2);
    assert!(matches!(
        frames.next_frame(Duration::from_secs(2)),
        RecvOutcome::Data(_)
    ));
}
