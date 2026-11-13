//! The simulated camera's counters, read as a consumer that keeps up: frames delivered are counted
//! exactly, no identity is skipped, and the achieved rate is the sensor's frame length over its
//! line time, within one line step.
//!
//! Not shown here: a held drop. Holding every buffer wedges the simulated stream (no frame comes
//! again, even after the buffers are released, and `held_drops` stays at zero), so the held
//! path has no working proof on the simulator. It is exercised only through
//! `CameraCounters::record_drop`, by its unit tests.

use std::time::Duration;

use styx_counters::{CameraCounters, DropReason, achieved_fps_milli, now_ns};
use styx_sim::{Scene, SimCamera};

/// The simulated sensor in mode 1 (640 by 40): its frame length in lines, and the line time.
const FRAME_LENGTH_LINES: u64 = 1831;
const LINE_TIME_NS: u64 = 9_100;
const FRAMES: u64 = 120;

#[test]
fn frames_are_counted_exactly_and_the_rate_is_the_sensors() {
    let mut camera = SimCamera::start(Scene::Pattern, 1).expect("start the simulated camera");
    let counters = CameraCounters::new();
    let mut ids = Vec::new();
    let mut polls = 0_u64;
    while (ids.len() as u64) < FRAMES {
        polls += 1;
        assert!(
            polls < 4 * FRAMES,
            "only {} frames in {polls} polls",
            ids.len()
        );
        if let Some(frame) = camera
            .next(Duration::from_millis(500))
            .expect("the simulated camera delivers")
        {
            counters.record_frame(frame.id(), frame.timestamp_ns(), now_ns());
            ids.push((frame.id(), frame.timestamp_ns()));
            camera.release(frame);
        }
    }

    let delivered = ids.len() as u64;
    assert_eq!(
        counters.frames(),
        delivered,
        "frames counted exactly as delivered"
    );
    assert_eq!(
        counters.drops(DropReason::Unattributed),
        0,
        "no identity was skipped"
    );
    let (first_id, first_ns) = ids[0];
    let (last_id, last_ns) = *ids.last().expect("frames");
    assert_eq!(last_id - first_id + 1, delivered, "consecutive identities");

    let fps = achieved_fps_milli(first_id, first_ns, last_id, last_ns);
    let achieved_interval_ns = 1_000_000_000_000_u64 / u64::from(fps);
    let expected_interval_ns = FRAME_LENGTH_LINES * LINE_TIME_NS;
    let difference = achieved_interval_ns.abs_diff(expected_interval_ns);
    assert!(
        difference <= LINE_TIME_NS,
        "achieved {achieved_interval_ns} ns per frame, sensor {expected_interval_ns} ns: {difference} ns apart"
    );
    println!(
        "delivered {delivered}, achieved {fps} milli-fps, interval {achieved_interval_ns} ns, sensor {expected_interval_ns} ns (difference {difference} ns, one line is {LINE_TIME_NS} ns)"
    );
}
