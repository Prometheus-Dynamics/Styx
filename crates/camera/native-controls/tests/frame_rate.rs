//! A frame rate is the frame length over the line time, in the mode's own numbers. A rate the
//! mode's frame lengths cannot reach is clamped and reported; nothing is a nominal table.

use lemnos_contract_camera_sensor::{GainRange, Mode};
use styx_native_controls::{Initial, NativeControls};

/// A mode whose shortest frame is `lines` lines of `line_time_ps`.
fn mode(lines: u32, line_time_ps: u32) -> Mode {
    Mode {
        line_time_ps,
        line_length: 1456,
        frame_length_min: lines,
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
        readout_ps: 0,
        ..Mode::default()
    }
}

fn initial(frame_rate_milli: u32) -> Initial {
    Initial {
        exposure_us: 3_000,
        gain_milli: 1_000,
        frame_rate_milli,
    }
}

#[test]
fn a_mode_whose_shortest_frame_is_114_80_fps_gives_114_80_when_120_is_asked() {
    // 1000 lines of 8.710801 ms each: 1e15 / (1000 * 8_710_801) = 114.800 fps.
    let m = mode(1_000, 8_710_801);
    let controls = NativeControls::new(&m, initial(120_000)).expect("controls");
    let rate = controls.initial_rate();
    assert_eq!(rate.lines, 1_000, "the shortest frame");
    assert_eq!(rate.achieved_milli, 114_800);
    assert!(
        rate.clamped,
        "120 fps is outside the mode's frame lengths: {rate:?}"
    );
}

#[test]
fn a_mode_whose_shortest_frame_is_137_76_fps_gives_120_within_0_06_percent_by_lengthening_it() {
    // 1000 lines of 7.257556 us each: 1e15 / (1000 * 7_257_556) = 137.760 fps at the shortest.
    let m = mode(1_000, 7_257_556);
    let controls = NativeControls::new(&m, initial(120_000)).expect("controls");
    let rate = controls.initial_rate();
    assert!(!rate.clamped, "{rate:?}");
    assert!(rate.lines > 1_000, "the frame is lengthened: {rate:?}");
    let error = rate.achieved_milli.abs_diff(120_000);
    assert!(
        u64::from(error) * 10_000 <= 120_000 * 6,
        "{} mfps is {error} off 120 000: more than 0.06%",
        rate.achieved_milli
    );
}

#[test]
fn a_zero_frame_rate_is_refused() {
    let m = mode(1_000, 7_257_556);
    assert!(NativeControls::new(&m, initial(0)).is_err());
}
