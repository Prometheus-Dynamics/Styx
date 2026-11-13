//! Refusals: a value outside the sensor's own limits, or one this path does not post, is refused
//! with nothing posted and the limit named.

use lemnos_contract_camera_sensor::{GainRange, Mode};
use styx_controls::{Control, ControlValue, Update, ValueError};
use styx_native_controls::{Error, GainLaw, Initial, Limit, NativeControls};

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
    let mut controls = NativeControls::new(&mode, START, GainLaw::Linear).unwrap();
    let mut sensor = Sensor::new(&mode);
    controls.prepare(&mut sensor).unwrap();
    for k in 0..=10 {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k).unwrap();
    }
    (controls, sensor)
}

#[test]
fn an_exposure_past_the_frame_is_refused_with_the_limit_named() {
    let (mut controls, mut sensor) = streaming();
    let posts = sensor.posts.len();
    let err = controls
        .request(&mut sensor, &[manual(Control::Exposure, 17_000, 12)])
        .unwrap_err();
    match err {
        Error::Limit(Limit {
            control: Control::Exposure,
            requested: 17_000,
            min: 1..,
            max,
            frame_lines: Some(1832),
        }) => assert!((16_440..=16_450).contains(&max), "max {max}"),
        other => panic!("expected an exposure limit, got {other:?}"),
    }
    assert!(err.to_string().contains("at a frame length of 1832 lines"));
    assert_eq!(sensor.posts.len(), posts, "nothing posted");
}

#[test]
fn a_gain_past_the_sensors_code_range_is_refused_not_clamped() {
    let (mut controls, mut sensor) = streaming();
    let err = controls
        .request(&mut sensor, &[manual(Control::Gain, 16_000, 12)])
        .unwrap_err();
    assert_eq!(
        err,
        Error::Limit(Limit {
            control: Control::Gain,
            requested: 16_000,
            min: 1_000,
            max: 15_938,
            frame_lines: None,
        })
    );
    assert_eq!(
        err.to_string(),
        "gain 16000 milli is outside the sensor's 1000..=15938 milli"
    );
    assert!(sensor.posts.iter().all(|&(_, id)| id != 12));
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
fn a_frame_rate_the_frame_length_cannot_reach_is_refused() {
    let (mut controls, mut sensor) = streaming();
    match controls.request(&mut sensor, &[manual(Control::FrameRate, 150_000, 12)]) {
        Err(Error::Limit(Limit {
            control: Control::FrameRate,
            requested: 150_000,
            ..
        })) => {}
        other => panic!("expected a frame rate limit, got {other:?}"),
    }
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
    let mut controls = NativeControls::new(&mode, START, GainLaw::Linear).unwrap();
    let mut sensor = Sensor::new(&mode);
    assert_eq!(
        controls.request(&mut sensor, &[manual(Control::Exposure, 9_000, 1)]),
        Err(Error::NotStreaming)
    );
}

#[test]
fn a_frame_rate_that_cuts_the_exposure_in_force_is_refused() {
    let mode = mode();
    let mut controls = NativeControls::new(&mode, START, GainLaw::Linear).unwrap();
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
    let posts = sensor.posts.len();
    let err = controls
        .request(&mut sensor, &[manual(Control::FrameRate, 120_000, 10)])
        .unwrap_err();
    match err {
        Error::Limit(Limit {
            control: Control::Exposure,
            requested: 12_000,
            frame_lines: Some(916),
            ..
        }) => {}
        other => panic!("expected the exposure in force to be refused, got {other:?}"),
    }
    assert_eq!(sensor.posts.len(), posts);
}

#[test]
fn an_initial_value_the_sensor_cannot_take_is_refused_at_setup() {
    let err = NativeControls::new(
        &mode(),
        Initial {
            exposure_us: 17_000,
            ..START
        },
        GainLaw::Linear,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Limit(Limit {
            control: Control::Exposure,
            requested: 17_000,
            ..
        })
    ));
}
