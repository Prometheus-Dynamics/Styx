//! A lost resume: the device was bound again and the frame sequence restarts at zero. The controls
//! post what was asked for again, forget what the old sequence had in force, and report nothing
//! as applied until a post has landed on the new sequence.

mod common;

use common::{START, Sensor, code_for_us, mode, update};
use styx_controls::Control;
use styx_native_controls::NativeControls;

#[test]
fn a_restart_posts_what_was_asked_and_reports_nothing_until_a_post_lands() {
    let m = mode(2, 1, 2);
    let mut controls = NativeControls::new(&m, START).expect("controls");
    let mut sensor = Sensor::new(&m);
    controls.prepare(&mut sensor).expect("prepare");

    // The old sequence: frames 0..=20, with an exposure of 12 ms asked for in frame 3.
    for k in 0..=20u64 {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k).expect("frame");
        if k == 3 {
            controls
                .request(&mut sensor, &[update(Control::Exposure, 12_000, 6)])
                .expect("request");
        }
    }
    assert!(controls.exposure_report().applied_us.is_some());

    // The device is bound again: a new sensor instance, its values its own, and the sequence
    // restarts at zero. Until the controls are posted there is nothing applied, and what was
    // asked for is kept.
    controls.restart();
    let report = controls.exposure_report();
    assert_eq!(report.applied_us, None, "nothing is applied before the post lands");
    assert_eq!(report.requested_us, 12_000, "what was asked for is kept");
    let mut sensor = Sensor::new(&m);
    controls.prepare(&mut sensor).expect("prepare after the bind");
    assert_eq!(
        sensor.exposure_at(0),
        code_for_us(12_000) as u32,
        "the value asked for is posted again, for frame 0"
    );

    // The new sequence: frame 0 has the posted value in force, so its report is known again.
    sensor.frame = 0;
    controls.frame_start(&mut sensor, 0).expect("frame 0");
    assert!(controls.exposure_report().applied_us.is_some());
    // Frames from zero go on, and a new request is attributed on the new sequence.
    for k in 1..=8u64 {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k).expect("frame");
    }
    controls
        .request(&mut sensor, &[update(Control::Exposure, 9_000, 10)])
        .expect("request on the new sequence");
    assert_eq!(controls.exposure_report().requested_us, 9_000);
}
