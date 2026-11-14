//! A report is attributed to the frame it describes, or it is unknown.
//!
//! The sensor's in-force report has no frame in it: it is the driver's values as it holds them
//! now. A post made in a frame lands `delay` frames later, so a report read while a post is on its
//! way describes a later frame than the one it is read in. Styx keeps the report for a frame only
//! when every post of the control has landed by that frame; otherwise that frame's applied value is
//! unknown. The sensor's landing (`post_values_at`) is what makes the posts' frames exact, also when
//! the receiver has counted frames the session has not read yet.

mod common;

use common::{Run, START, Sensor, code_for_us, mode, report_us, update};
use lemnos_contract_camera_sensor::Mode;
use styx_controls::{Control, Update};
use styx_native_controls::{Error, NativeControls};

/// Runs frames `0..=end` with the receiver `ahead` frames past the session: when the session reads
/// frame `k`, the receiver has already counted frames up to `k + ahead` (they are queued). The
/// request is made in frame `request_at`.
fn run_ahead(
    mode: &Mode,
    ahead: u64,
    request_at: u64,
    updates: &[Update],
    end: u64,
) -> Result<Run, Error> {
    let mut controls = NativeControls::new(mode, START)?;
    let mut sensor = Sensor::new(mode);
    controls.prepare(&mut sensor)?;
    let mut landings = Vec::new();
    for k in 0..=end {
        sensor.frame = k + ahead;
        controls.frame_start(&mut sensor, k)?;
        if k == request_at {
            let got = controls.request(&mut sensor, updates)?;
            landings = got
                .iter()
                .map(|l| (l.control, l.requested, l.frame))
                .collect();
        }
    }
    Ok(Run {
        sensor,
        controls,
        landings,
    })
}

#[test]
fn a_receiver_run_several_frames_ahead_still_shows_each_frames_own_value() {
    // Exposure delay 2, gain delay 1: a post made in frame k lands on k+2 (exposure) and k+1 (gain),
    // and the receiver is `ahead` frames past the session, so the real landing is later than the
    // session's frame. Every frame's metadata must show the value the sensor had in force on that
    // frame, and the request's landing must be where the sensor put it.
    let delays = mode(2, 1, 2);
    let initial = code_for_us(8_000) as u32;
    let stepped = code_for_us(12_000) as u32;
    for ahead in [1u64, 2, 3, 5] {
        for step in [10u64, 11, 17] {
            let end = step + 12;
            let run = run_ahead(
                &delays,
                ahead,
                step,
                &[update(Control::Exposure, 12_000, step)],
                end,
            )
            .unwrap_or_else(|e| panic!("ahead {ahead}, step {step}: {e}"));

            // The post was made in the frame the receiver was in: its landing is that frame plus
            // the exposure delay.
            let landing = step + ahead + 2;
            assert_eq!(
                run.landings,
                [(Control::Exposure, step, landing)],
                "ahead {ahead}, step {step}: the landing names the frame the sensor launched in"
            );
            for k in 0..=end {
                let want = if k < landing { initial } else { stepped };
                assert_eq!(
                    run.sensor.exposure_at(k),
                    want,
                    "ahead {ahead}: the sensor's exposure on frame {k}"
                );
                assert_eq!(
                    run.controls.metadata(k, 0).exposure_us,
                    report_us(u64::from(want)),
                    "ahead {ahead}, step {step}: metadata on frame {k} shows the value in force on it"
                );
            }
        }
    }
}

#[test]
fn a_report_for_a_frame_with_a_post_still_on_its_way_is_unknown_until_it_lands() {
    // Exposure delay 2: the post made in frame 5 lands on frame 7. The report read in frames 5
    // and 6 describes the new value, which is not in force until 7: unknown there, known on 7.
    let delays = mode(2, 2, 2);
    let mut controls = NativeControls::new(&delays, START).expect("controls");
    let mut sensor = Sensor::new(&delays);
    controls.prepare(&mut sensor).expect("prepare");
    let mut applied = Vec::new();
    for k in 0..=8 {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k).expect("frame start");
        if k == 5 {
            controls
                .request(&mut sensor, &[update(Control::Exposure, 12_000, 5)])
                .expect("request");
        }
        applied.push(controls.exposure_report().applied_us);
    }
    let known = |us: u32| Some(us);
    assert_eq!(applied[0], known(report_us(code_for_us(8_000))));
    assert_eq!(applied[4], known(report_us(code_for_us(8_000))));
    assert_eq!(applied[5], None, "frame 5: the post is on its way");
    assert_eq!(applied[6], None, "frame 6: still on its way");
    assert_eq!(applied[7], known(report_us(code_for_us(12_000))));
    // The metadata shows the value in force on each frame, known or not.
    assert_eq!(
        controls.metadata(6, 0).exposure_us,
        report_us(code_for_us(8_000))
    );
    assert_eq!(
        controls.metadata(7, 0).exposure_us,
        report_us(code_for_us(12_000))
    );
}

#[test]
fn a_sensor_with_no_landings_reports_nothing_it_cannot_tie_to_a_frame() {
    // The sensor refuses landings, so its posts have no landing from the sensor: from the first
    // post on, no report can be tied to a frame, and every applied value is unknown.
    let delays = mode(0, 0, 0);
    let mut controls = NativeControls::new(&delays, START).expect("controls");
    let mut sensor = Sensor::new(&delays);
    sensor.refuse_landings = true;
    controls.prepare(&mut sensor).expect("prepare");
    let mut applied = Vec::new();
    for k in 0..=6 {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k).expect("frame start");
        if k == 3 {
            controls
                .request(&mut sensor, &[update(Control::Exposure, 12_000, 3)])
                .expect("request");
        }
        applied.push(controls.exposure_report().applied_us);
    }
    // Before the first post, the initial values are in force on every frame: known.
    assert!(applied[..3].iter().all(Option::is_some), "{applied:?}");
    assert!(applied[3..].iter().all(Option::is_none), "{applied:?}");
    // The request was made with no landing from the sensor, so the schedule is Styx's arithmetic.
    assert_eq!(
        controls.metadata(3, 0).exposure_us,
        report_us(code_for_us(12_000))
    );
}
