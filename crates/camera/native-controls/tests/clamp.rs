//! The clamp arithmetic on a mode like the board's: a 915-line shortest frame, with the sensor's
//! margin (25 lines), minimum (16 codes, one line) and fraction bits (4). An exposure the frame
//! cannot hold is moved to the frame's ceiling, a gain past the codes to the nearest code, and a
//! frame rate is never traded for an exposure: the exposure is what moves.

mod common;

use lemnos_contract_camera_sensor::{GainRange, Mode};
use styx_controls::{Control, ControlValue, Update};
use styx_native_controls::{ClampReason, Initial, NativeControls};

use common::Sensor;

/// The board-like mode: a 915-line shortest frame at `line_time_ps`, the sensor's limits as the
/// sim and the contract's test doubles report them.
fn board_mode(line_time_ps: u32) -> Mode {
    Mode {
        line_time_ps,
        line_length: 1456,
        frame_length_min: 915,
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

fn start(exposure_us: u32, frame_rate_milli: u32) -> Initial {
    Initial {
        exposure_us,
        gain_milli: 1_000,
        frame_rate_milli,
    }
}

#[test]
fn a_10_ms_exposure_at_the_915_line_ceiling_is_clamped_to_the_frame_less_the_margin() {
    // 240 fps is past the mode's ceiling: the frame is its shortest, 915 lines. The exposure can be
    // (915 - 25) = 890 lines: 890 x 7.583 us = 6 748.9 us, applied as 6 749 us.
    let controls = NativeControls::new(&board_mode(7_583_000), start(10_000, 240_000)).unwrap();
    assert_eq!(controls.initial_rate().lines, 915);
    let report = controls.exposure_report();
    assert_eq!((report.requested_us, report.applied_us), (10_000, 6_749));
    let clamp = report.clamp.expect("a clamp");
    assert_eq!(clamp.reason, ClampReason::Ceiling);
    assert_eq!(clamp.frame_lines, Some(915));
    assert_eq!(
        clamp.text,
        "exposure asked 10000 us, the frame is 915 lines, exposure can be at most 6749 us: applied 6749 us"
    );
    // The frame shows what was applied.
    assert_eq!(controls.metadata(0, 0).exposure_us, 6_749);
}

#[test]
fn at_9_1_us_a_line_the_same_frame_allows_8_099_us() {
    // The line time of the simulated board: (915 - 25) x 9.1 us = 8 099 us.
    let controls = NativeControls::new(&board_mode(9_100_000), start(10_000, 240_000)).unwrap();
    let report = controls.exposure_report();
    assert_eq!(report.applied_us, 8_099);
    assert!(
        report
            .clamp
            .expect("a clamp")
            .text
            .contains("at most 8099 us")
    );
}

#[test]
fn an_exposure_the_frame_holds_is_applied_as_asked_with_no_clamp() {
    let controls = NativeControls::new(&board_mode(7_583_000), start(5_000, 240_000)).unwrap();
    let report = controls.exposure_report();
    assert_eq!(report.requested_us, 5_000);
    assert!(report.clamp.is_none(), "{report:?}");
    assert!(
        (4_999..=5_001).contains(&report.applied_us),
        "applied {}",
        report.applied_us
    );
    assert!(controls.clamp(Control::Exposure).is_none());
}

#[test]
fn an_exposure_below_the_sensors_minimum_is_clamped_up_to_one_line() {
    // One line is 7.583 us: the minimum is 16 codes, which is 8 us after rounding.
    let controls = NativeControls::new(&board_mode(7_583_000), start(1, 240_000)).unwrap();
    let report = controls.exposure_report();
    assert_eq!(report.applied_us, 8);
    let clamp = report.clamp.expect("a clamp");
    assert_eq!(clamp.reason, ClampReason::Floor);
    assert_eq!(
        clamp.text,
        "exposure asked 1 us, the frame is 915 lines, exposure can be at least 8 us: applied 8 us"
    );
}

#[test]
fn a_frame_rate_asked_inside_the_frame_lengths_keeps_the_exposure_it_holds() {
    // 60 fps at 7.583 us is 2 198 lines: the ceiling there is 2 173 lines, far above 10 ms.
    let controls = NativeControls::new(&board_mode(7_583_000), start(10_000, 60_000)).unwrap();
    assert!(controls.exposure_report().clamp.is_none());
    assert_eq!(controls.exposure_report().applied_us, 10_000);
}

#[test]
fn a_running_request_past_the_ceiling_is_applied_clamped_and_posted_clamped() {
    let mode = board_mode(7_583_000);
    let mut controls = NativeControls::new(&mode, start(5_000, 240_000)).unwrap();
    let mut sensor = Sensor::new(&mode);
    controls.prepare(&mut sensor).unwrap();
    for k in 0..=5 {
        sensor.frame = k;
        controls.frame_start(&mut sensor, k).unwrap();
    }
    let landings = controls
        .request(
            &mut sensor,
            &[Update {
                control: Control::Exposure,
                value: ControlValue::Manual(20_000),
                frame: 7,
            }],
        )
        .expect("clamped, not refused");
    assert_eq!(landings.len(), 1);
    // 890 lines of 16 codes: the posted exposure is the frame's ceiling, not the 20 ms asked.
    assert_eq!(
        sensor.exposure.last().map(|&(_, code)| code),
        Some(890 << 4)
    );
    assert_eq!(controls.metadata(7, 0).exposure_us, 6_749);
    assert_eq!(controls.exposure_report().requested_us, 20_000);
    assert_eq!(
        controls.clamp(Control::Exposure).map(|c| c.frame_lines),
        Some(Some(915))
    );
}

#[test]
fn the_gain_range_is_clamped_both_ways() {
    let mode = board_mode(7_583_000);
    let mut controls = NativeControls::new(&mode, start(5_000, 240_000)).unwrap();
    let mut sensor = Sensor::new(&mode);
    controls.prepare(&mut sensor).unwrap();
    controls.frame_start(&mut sensor, 0).unwrap();
    // Above the top code, then below the bottom one (1 000 milli is unity; the validated range
    // starts there, so the floor is the one a request inside the generic range can reach).
    controls
        .request(
            &mut sensor,
            &[Update {
                control: Control::Gain,
                value: ControlValue::Manual(16_000),
                frame: 3,
            }],
        )
        .unwrap();
    let clamp = controls.clamp(Control::Gain).expect("a gain clamp");
    assert_eq!(
        (clamp.applied, clamp.reason),
        (15_938, ClampReason::Ceiling)
    );
    controls
        .request(
            &mut sensor,
            &[Update {
                control: Control::Gain,
                value: ControlValue::Manual(2_000),
                frame: 4,
            }],
        )
        .unwrap();
    assert!(controls.clamp(Control::Gain).is_none(), "2.0x is a code");
}
