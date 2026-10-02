use super::*;
use Control::*;

fn delays(exposure: u32, gain: u32, frame_length: u32) -> Delays {
    Delays {
        exposure,
        analog_gain: gain,
        digital_gain: gain,
        frame_length,
    }
}

fn initial() -> ControlSet {
    ControlSet::new()
        .with(FrameLength, 1822)
        .with(Exposure, 642)
        .with(AnalogGain, 16)
}

fn scheduler() -> ControlScheduler {
    ControlScheduler::new(delays(2, 1, 2), initial())
}

/// Run frame starts `from..=to`, collecting the non-empty batches.
fn run(s: &mut ControlScheduler, from: u64, to: u64) -> Vec<IssueBatch> {
    (from..=to)
        .map(|f| s.frame_start(f))
        .filter(|b| !b.controls.is_empty())
        .collect()
}

#[test]
fn values_issue_at_frame_minus_delay_and_land_on_the_frame() {
    let mut s = scheduler();
    s.frame_start(0);
    let landings = s.request(
        10,
        &ControlSet::new().with(Exposure, 500).with(AnalogGain, 32),
    );
    assert!(landings.iter().all(|l| l.frame == 10 && !l.late()));
    let batches = run(&mut s, 1, 12);
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].frame, Some(8));
    assert_eq!(batches[0].controls, ControlSet::new().with(Exposure, 500));
    assert_eq!(batches[0].lands(Exposure), Some(10));
    assert_eq!(batches[1].frame, Some(9));
    assert_eq!(batches[1].controls, ControlSet::new().with(AnalogGain, 32));
    assert_eq!(batches[1].lands(AnalogGain), Some(10));
    assert_eq!(s.applied(9).values, initial());
    assert_eq!(
        s.applied(10).values,
        initial().with(Exposure, 500).with(AnalogGain, 32)
    );
    assert_eq!(s.applied(11).values, s.applied(10).values);
}

#[test]
fn predictions_include_pending_requests() {
    let mut s = scheduler();
    s.frame_start(0);
    s.request(5, &ControlSet::new().with(Exposure, 100));
    s.request(7, &ControlSet::new().with(Exposure, 200));
    assert_eq!(s.predicted(4).get(Exposure), Some(642));
    assert_eq!(s.predicted(5).get(Exposure), Some(100));
    assert_eq!(s.predicted(6).get(Exposure), Some(100));
    assert_eq!(s.predicted(7).get(Exposure), Some(200));
}

#[test]
fn late_requests_land_as_early_as_possible() {
    let mut s = scheduler();
    run(&mut s, 0, 9);
    let l = s.request(
        10,
        &ControlSet::new().with(Exposure, 500).with(AnalogGain, 32),
    );
    // Next issue opportunity is frame 10: exposure lands 12, gain 11.
    assert_eq!(
        l[0],
        Landing {
            control: Exposure,
            value: 500,
            requested: 10,
            frame: 12
        }
    );
    assert!(l[0].late());
    assert_eq!(l[1].frame, 11);
    let b = s.frame_start(10);
    assert_eq!(b.lands(Exposure), Some(12));
    assert_eq!(b.lands(AnalogGain), Some(11));
    assert_eq!(s.applied(11).values.get(Exposure), Some(642));
    assert_eq!(s.applied(11).values.get(AnalogGain), Some(32));
    assert_eq!(s.applied(12).values.get(Exposure), Some(500));
}

#[test]
fn issue_now_writes_within_the_current_frame() {
    let mut s = scheduler();
    run(&mut s, 0, 9);
    s.request(11, &ControlSet::new().with(Exposure, 500));
    let b = s.issue_now();
    assert_eq!(b.frame, Some(9));
    assert_eq!(b.lands(Exposure), Some(11));
    assert!(s.frame_start(10).controls.is_empty());
    assert_eq!(s.applied(11).values.get(Exposure), Some(500));
}

#[test]
fn several_late_values_collapse_to_the_latest() {
    let mut s = scheduler();
    run(&mut s, 0, 5);
    s.request(5, &ControlSet::new().with(Exposure, 100));
    s.request(6, &ControlSet::new().with(Exposure, 200));
    s.request(9, &ControlSet::new().with(Exposure, 300));
    let b = s.frame_start(6);
    assert_eq!(b.controls.get(Exposure), Some(200));
    assert_eq!(b.lands(Exposure), Some(8));
    let b = s.frame_start(7);
    assert_eq!(b.controls.get(Exposure), Some(300));
    assert_eq!(b.lands(Exposure), Some(9));
    assert_eq!(s.applied(8).values.get(Exposure), Some(200));
    assert_eq!(s.applied(9).values.get(Exposure), Some(300));
}

#[test]
fn a_later_request_for_the_same_frame_replaces_the_earlier() {
    let mut s = scheduler();
    s.frame_start(0);
    s.request(6, &ControlSet::new().with(AnalogGain, 20));
    s.request(6, &ControlSet::new().with(AnalogGain, 40));
    let b = run(&mut s, 1, 6);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].controls.get(AnalogGain), Some(40));
}

#[test]
fn requests_before_streaming_apply_from_frame_zero() {
    let mut s = scheduler();
    let l = s.request(
        0,
        &ControlSet::new().with(Exposure, 300).with(AnalogGain, 64),
    );
    assert!(l.iter().all(|l| l.frame == 0));
    let b = s.issue_now();
    assert_eq!(b.frame, None);
    assert_eq!(
        b.controls,
        ControlSet::new().with(Exposure, 300).with(AnalogGain, 64)
    );
    assert_eq!(s.applied(0).values.get(Exposure), Some(300));
    // Requests for frames at or beyond the delay are scheduled normally.
    let l = s.request(3, &ControlSet::new().with(Exposure, 400));
    assert_eq!(l[0].frame, 3);
    assert!(s.issue_now().controls.is_empty());
    assert!(s.frame_start(0).controls.is_empty());
    assert_eq!(s.frame_start(1).lands(Exposure), Some(3));
}

#[test]
fn pre_start_values_not_issued_in_time_go_out_with_frame_zero() {
    let mut s = scheduler();
    s.request(0, &ControlSet::new().with(Exposure, 300));
    let b = s.frame_start(0);
    assert_eq!(b.lands(Exposure), Some(2));
    assert_eq!(s.applied(1).values.get(Exposure), Some(642));
    assert_eq!(s.applied(2).values.get(Exposure), Some(300));
}

#[test]
fn dropped_frame_starts_issue_everything_due() {
    let mut s = scheduler();
    s.frame_start(5);
    s.request(8, &ControlSet::new().with(Exposure, 111));
    s.request(9, &ControlSet::new().with(AnalogGain, 22));
    // Frame starts 6..=8 were missed.
    let b = s.frame_start(9);
    assert_eq!(
        b.controls,
        ControlSet::new().with(Exposure, 111).with(AnalogGain, 22)
    );
    assert_eq!(b.lands(Exposure), Some(11));
    assert_eq!(b.lands(AnalogGain), Some(10));
}

#[test]
fn zero_delay_controls_land_on_the_issue_frame() {
    let mut s = ControlScheduler::new(delays(0, 0, 0), initial());
    s.frame_start(0);
    s.request(4, &ControlSet::new().with(Exposure, 10));
    assert!(run(&mut s, 1, 3).is_empty());
    let b = s.frame_start(4);
    assert_eq!(b.lands(Exposure), Some(4));
}

fn limited(fl_delay: u32) -> ControlScheduler {
    ControlScheduler::new(delays(2, 1, fl_delay), initial()).with_exposure_limit(ExposureLimit {
        min: 1,
        margin: 12,
        fraction_bits: 0,
    })
}

#[test]
fn exposure_is_clamped_to_the_frame_length_it_lands_on() {
    let mut s = limited(2);
    s.frame_start(0);
    s.request(4, &ControlSet::new().with(Exposure, 5000));
    let b = run(&mut s, 1, 4);
    assert_eq!(b[0].controls.get(Exposure), Some(1810));
    assert_eq!(b[0].exposure_clamped_from, Some(5000));
    assert_eq!(s.applied(4).values.get(Exposure), Some(1810));
}

#[test]
fn a_longer_frame_requested_together_allows_a_longer_exposure() {
    let mut s = limited(2);
    s.frame_start(0);
    s.request(
        4,
        &ControlSet::new()
            .with(Exposure, 5000)
            .with(FrameLength, 6000),
    );
    let b = run(&mut s, 1, 4);
    assert_eq!(
        b[0].controls,
        ControlSet::new()
            .with(FrameLength, 6000)
            .with(Exposure, 5000)
    );
    assert_eq!(b[0].exposure_clamped_from, None);
}

#[test]
fn a_pending_frame_length_with_shorter_delay_counts_for_the_clamp() {
    // Frame length delay 1 < exposure delay 2: the frame length is issued a frame later but
    // lands on the same frame as the exposure.
    let mut s = limited(1);
    s.frame_start(0);
    s.request(
        4,
        &ControlSet::new()
            .with(Exposure, 5000)
            .with(FrameLength, 6000),
    );
    let b = run(&mut s, 1, 4);
    assert_eq!(b[0].frame, Some(2));
    assert_eq!(b[0].controls, ControlSet::new().with(Exposure, 5000));
    assert_eq!(b[1].frame, Some(3));
    assert_eq!(b[1].controls, ControlSet::new().with(FrameLength, 6000));
    assert_eq!(s.applied(4).values.get(FrameLength), Some(6000));
}

#[test]
fn a_shorter_frame_clamps_the_exposure_landing_with_it() {
    let mut s = limited(2);
    s.frame_start(0);
    s.request(3, &ControlSet::new().with(FrameLength, 910));
    let b = s.frame_start(1);
    assert_eq!(b.lands(FrameLength), Some(3));
    // Exposure 1000 requested for frame 3 no longer fits 910 - 12 lines.
    s.request(3, &ControlSet::new().with(Exposure, 1000));
    // Too late for frame 3 (issue would have been frame 1); lands on 4 with frame length 910.
    let b = s.frame_start(2);
    assert_eq!(b.lands(Exposure), Some(4));
    assert_eq!(b.controls.get(Exposure), Some(898));
}

#[test]
fn fractional_exposure_limits_scale_with_fraction_bits() {
    let mut s =
        ControlScheduler::new(delays(2, 1, 2), initial()).with_exposure_limit(ExposureLimit {
            min: 1,
            margin: 12,
            fraction_bits: 4,
        });
    s.frame_start(0);
    s.request(2, &ControlSet::new().with(Exposure, 0));
    assert_eq!(s.issue_now().controls.get(Exposure), Some(16));
}

#[test]
fn reports_override_predictions_and_flag_mismatches() {
    let mut s = scheduler();
    s.frame_start(0);
    s.request(5, &ControlSet::new().with(Exposure, 500));
    run(&mut s, 1, 6);
    // The sensor actually applied the new exposure one frame late.
    let m = s.report(5, &ControlSet::new().with(Exposure, 642));
    assert_eq!(
        m,
        vec![Mismatch {
            frame: 5,
            control: Exposure,
            predicted: Some(500),
            reported: 642
        }]
    );
    assert!(
        s.report(6, &ControlSet::new().with(Exposure, 500))
            .is_empty()
    );
    let a = s.applied(5);
    assert_eq!(a.values.get(Exposure), Some(642));
    assert_eq!(a.reported, ControlSet::new().with(Exposure, 642));
    assert_eq!(a.values.get(AnalogGain), Some(16));
    assert!(s.applied(6).reported.get(Exposure).is_some());
    assert!(s.applied(7).reported.is_empty());
    assert_eq!(s.applied(7).values.get(Exposure), Some(500));
}

#[test]
fn a_reported_value_persists_until_the_next_change() {
    let mut s = scheduler();
    run(&mut s, 0, 3);
    // Someone else changed the gain behind the scheduler's back.
    s.report(3, &ControlSet::new().with(AnalogGain, 99));
    assert_eq!(s.predicted(8).get(AnalogGain), Some(99));
    s.request(8, &ControlSet::new().with(AnalogGain, 20));
    run(&mut s, 4, 8);
    assert_eq!(s.applied(7).values.get(AnalogGain), Some(99));
    assert_eq!(s.applied(8).values.get(AnalogGain), Some(20));
}

#[test]
fn history_is_pruned_but_current_values_kept() {
    let mut s = scheduler();
    s.frame_start(0);
    s.request(3, &ControlSet::new().with(Exposure, 700));
    run(&mut s, 1, 200);
    assert_eq!(s.applied(200).values.get(Exposure), Some(700));
    assert!(s.committed.iter().all(|m| m.len() <= 2));
    assert_eq!(s.current_frame(), Some(200));
}

#[test]
fn max_delay_and_accessors() {
    let s = ControlScheduler::new(delays(2, 1, 3), ControlSet::new());
    assert_eq!(s.max_delay(), 3);
    assert_eq!(s.delay(AnalogGain), 1);
    assert_eq!(s.predicted(10), ControlSet::new());
    let mut c = ControlSet::new().with(Exposure, 1);
    c.clear(Exposure);
    assert!(c.is_empty());
}

#[test]
fn request_now_writes_what_is_due_in_the_current_frame() {
    // Exposure 2, gain 1, frame length 2 frames.
    let mut s = scheduler();
    s.frame_start(5);
    // Wanted from 7: exposure and frame length go out now (during 5), gain at 6's start.
    let set = ControlSet::new()
        .with(Exposure, 500)
        .with(AnalogGain, 32)
        .with(FrameLength, 1900);
    let (landings, batch) = s.request_now(7, &set);
    assert!(
        landings.iter().all(|l| l.frame == 7 && !l.late()),
        "{landings:?}"
    );
    assert_eq!(batch.frame, Some(5));
    assert_eq!(
        batch.controls,
        ControlSet::new()
            .with(Exposure, 500)
            .with(FrameLength, 1900)
    );
    assert_eq!(
        s.frame_start(6).controls,
        ControlSet::new().with(AnalogGain, 32)
    );
    assert_eq!(s.applied(7).values, set);
    assert_eq!(s.applied(6).values, initial());
    // Too late for 6: lands on 7 (written now, in 5).
    let mut s = scheduler();
    s.frame_start(5);
    let (landings, _) = s.request_now(6, &ControlSet::new().with(Exposure, 400));
    assert_eq!(landings[0].frame, 7);
    // Before streaming: values for frame 0 are written at once.
    let mut s = scheduler();
    let (landings, batch) = s.request_now(0, &ControlSet::new().with(Exposure, 300));
    assert_eq!(
        (landings[0].frame, batch.controls.get(Exposure)),
        (0, Some(300))
    );
    assert!(s.frame_start(0).controls.is_empty());
}
