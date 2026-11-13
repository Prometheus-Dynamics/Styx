//! Refusals: a value outside the sensor's own limits, or one this path does not post, is refused
//! with nothing posted and the limit named.

use lemnos_contract_camera_sensor::{GainRange, Mode};
use styx_controls::{Control, ControlValue, Update, ValueError};
use styx_native_controls::{ClampReason, Error, Initial, NativeControls};

mod common;

use common::Sensor;

fn mode() -> Mode {
    Mode {
        line_time_ps: 9_100_000,
        line_length: 1456,
        frame_length_min: 900,
        frame_length_max: 52_000,
        exposure_min: 16,
        exposure_margin: 25,
        exposure_fraction_bits: 4,
        analogue_gain: GainRange {
            min: 16,
            max: 255,
            unity: 16,
            step: 1,
            law: lemnos_contract_camera_sensor::GainLaw::Linear,
        },
        exposure_delay: 2,
        gain_delay: 2,
        frame_length_delay: 2,
        ..Mode::default()
    }
}

const START: Initial = Initial {
    exposure_us: 8_000,
    gain_milli: 1_000,
    frame_rate_milli: 60_000,
};

fn manual(control: Control, value: u32, frame: u64) -> Update {
    Update {
        control,
        value: ControlValue::Manual(value),
        frame,
    }
}

/// A controls and sensor that are streaming, at frame 10 (the frame the test asks in).
fn streaming() -> (NativeControls, Sensor) {
    let mode = mode();
    let mut controls = NativeControls::new(&mode, START).unwrap();
    let mut sensor = Sensor::new(&mode);
    controls.prepare(&mut sensor).unwrap();
    for k in 0..=10 {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k).unwrap();
    }
    (controls, sensor)
}

#[test]
fn an_exposure_past_the_frame_is_clamped_to_its_ceiling_and_the_reason_is_kept() {
    // 60 fps is a frame of 1832 lines: the ceiling is (1832 - 25) lines of 9.1 us, 16 444 us.
    let (mut controls, mut sensor) = streaming();
    let landings = controls
        .request(&mut sensor, &[manual(Control::Exposure, 17_000, 12)])
        .expect("clamped, not refused");
    assert_eq!(landings.len(), 1);
    let report = controls.exposure_report();
    assert_eq!(report.requested_us, 17_000);
    assert_eq!(report.applied_us, 16_444);
    let clamp = report.clamp.expect("a clamp");
    assert_eq!(clamp.reason, ClampReason::Ceiling);
    assert_eq!(clamp.frame_lines, Some(1832));
    assert_eq!(clamp.applied, 16_444);
    assert!(
        clamp
            .text
            .contains("the frame is 1832 lines, exposure can be at most 16444 us"),
        "{}",
        clamp.text
    );
    // The frame shows the applied value, not the request.
    assert_eq!(controls.metadata(12, 0).exposure_us, 16_444);
}

#[test]
fn a_gain_past_the_sensors_codes_is_clamped_to_the_nearest_and_the_reason_is_kept() {
    // The top code is 255 at unity 16: 15 938 milli is its gain.
    let (mut controls, mut sensor) = streaming();
    controls
        .request(&mut sensor, &[manual(Control::Gain, 16_000, 12)])
        .expect("clamped, not refused");
    assert_eq!(controls.metadata(12, 0).analogue_gain_milli, 15_938);
    let clamp = controls.clamp(Control::Gain).expect("a gain clamp");
    assert_eq!(clamp.reason, ClampReason::Ceiling);
    assert_eq!((clamp.requested, clamp.applied), (16_000, 15_938));
    assert_eq!(
        clamp.text,
        "gain asked 16000 milli, gain can be at most 15938 milli: applied 15938 milli"
    );
    assert!(controls.clamp(Control::Exposure).is_none());
}

#[test]
fn a_value_outside_the_generic_range_is_a_value_error() {
    let (mut controls, mut sensor) = streaming();
    assert_eq!(
        controls.request(&mut sensor, &[manual(Control::Gain, 500, 12)]),
        Err(Error::Value(ValueError::OutOfRange {
            min: 1_000,
            max: 16_000,
            value: 500
        }))
    );
}

#[test]
fn a_frame_rate_the_frame_length_cannot_reach_is_clamped_and_reported() {
    // 150 fps is a frame of 733 lines; the shortest frame here is 900 lines, which runs at
    // 1e15 / (900 * 9.1e6) = 122.100 fps. The ask is clamped to it, and the landing says so.
    // The exposure in force (8 ms) would not fit the clamped frame, so this asks for a shorter one
    // too: a clamp that cuts the exposure in force is refused, as the next test says.
    let (mut controls, mut sensor) = streaming();
    let landings = controls
        .request(
            &mut sensor,
            &[
                manual(Control::FrameRate, 150_000, 12),
                manual(Control::Exposure, 5_000, 12),
            ],
        )
        .expect("clamped, not refused");
    let landing = landings
        .iter()
        .find(|l| l.control == Control::FrameRate)
        .expect("a frame rate landing");
    assert!(landing.clamped);
    assert_eq!(landing.achieved_milli, Some(122_100));
    assert_eq!(sensor.frame_length.last().map(|&(_, fl)| fl), Some(900));
}

#[test]
fn a_frame_rate_the_frame_length_reaches_is_not_clamped() {
    // 120 fps at 9.1 us a line: 916 lines, which is in the frame lengths (900 to 52000).
    let (mut controls, mut sensor) = streaming();
    let landings = controls
        .request(&mut sensor, &[manual(Control::FrameRate, 120_000, 12)])
        .expect("in limits");
    let landing = landings.iter().next().expect("one landing");
    assert!(!landing.clamped);
    // 916 lines at 9.1 us is 119.967 fps: within 0.06% of 120.
    assert_eq!(landing.achieved_milli, Some(119_967));
}

#[test]
fn auto_unsupported_and_duplicate_requests_are_refused() {
    let (mut controls, mut sensor) = streaming();
    assert_eq!(
        controls.request(
            &mut sensor,
            &[Update {
                control: Control::Exposure,
                value: ControlValue::Auto,
                frame: 12,
            }]
        ),
        Err(Error::Auto(Control::Exposure))
    );
    assert_eq!(
        controls.request(
            &mut sensor,
            &[Update {
                control: Control::WhiteBalance,
                value: ControlValue::Manual(4_000),
                frame: 12,
            }]
        ),
        Err(Error::Unsupported(Control::WhiteBalance))
    );
    assert_eq!(
        controls.request(
            &mut sensor,
            &[
                manual(Control::Exposure, 9_000, 12),
                manual(Control::Exposure, 9_500, 12),
            ]
        ),
        Err(Error::Duplicate(Control::Exposure))
    );
    assert!(sensor.posts.iter().all(|&(_, id)| id != 12));
}

#[test]
fn a_request_before_the_first_frame_is_refused() {
    let mode = mode();
    let mut controls = NativeControls::new(&mode, START).unwrap();
    let mut sensor = Sensor::new(&mode);
    assert_eq!(
        controls.request(&mut sensor, &[manual(Control::Exposure, 9_000, 1)]),
        Err(Error::NotStreaming)
    );
}

#[test]
fn a_frame_rate_that_cuts_the_exposure_in_force_clamps_the_exposure_and_keeps_the_rate() {
    let mode = mode();
    let mut controls = NativeControls::new(&mode, START).unwrap();
    let mut sensor = Sensor::new(&mode);
    controls.prepare(&mut sensor).unwrap();
    for k in 0..=9 {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k).unwrap();
        if k == 8 {
            // 12 ms is 1319 lines: longer than a 120 fps frame (916 lines less 25) allows.
            controls
                .request(&mut sensor, &[manual(Control::Exposure, 12_000, 10)])
                .unwrap();
        }
    }
    // Frame rate has priority: the frame is 916 lines, not lengthened for the 12 ms exposure. The
    // exposure is moved to the ceiling of that frame, (916 - 25) lines of 9.1 us, 8 108 us, from frame 12.
    controls
        .request(&mut sensor, &[manual(Control::FrameRate, 120_000, 10)])
        .expect("clamped, not refused");
    assert_eq!(sensor.frame_length.last().map(|&(_, fl)| fl), Some(916));
    // The request is posted in frame 9 (the frame started), so it lands on frame 9 + 2 = 11, as the
    // frame length does; frame 10 still shows the 12 ms it was given.
    assert_eq!(controls.metadata(11, 0).exposure_us, 8_108);
    assert_eq!(controls.metadata(10, 0).exposure_us, 12_000);
    let report = controls.exposure_report();
    assert_eq!(report.requested_us, 12_000);
    assert_eq!(report.applied_us, 8_108);
    let clamp = report.clamp.expect("the exposure in force was clamped");
    assert_eq!(clamp.frame_lines, Some(916));
    assert_eq!(clamp.reason, ClampReason::Ceiling);
}

#[test]
fn an_initial_exposure_the_frame_cannot_hold_is_clamped_at_setup_with_the_reason() {
    let controls = NativeControls::new(
        &mode(),
        Initial {
            exposure_us: 17_000,
            ..START
        },
    )
    .expect("clamped, not refused");
    let report = controls.exposure_report();
    assert_eq!((report.requested_us, report.applied_us), (17_000, 16_444));
    let clamp = report.clamp.expect("a clamp");
    assert_eq!(clamp.frame_lines, Some(1832));
    assert_eq!(controls.metadata(0, 0).exposure_us, 16_444);
}
