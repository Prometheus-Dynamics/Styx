//! A frame rate is the frame length over the line time, in the mode's own numbers. A rate the
//! mode's frame lengths cannot reach is clamped by the driver and reported; nothing is a nominal
//! table.

mod common;

use lemnos_contract_camera_sensor::{GainRange, Mode, fps_milli_of, frame_length_for_fps_milli};
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
    let mut controls = NativeControls::new(&m, initial(120_000)).expect("controls");
    // Styx asks for the nearest frame (957 lines); the driver clamps it to the mode's shortest and
    // reports that, which is read at the first frame.
    let mut sensor = common::Sensor::new(&m);
    controls.prepare(&mut sensor).expect("prepare");
    controls.frame_start(&mut sensor, 0).expect("frame start");
    let rate = controls.initial_rate();
    assert_eq!(rate.lines, 1_000, "the shortest frame");
    assert_eq!(rate.achieved_milli, fps_milli_of(8_710_801, 1_000).unwrap());
    assert!(
        rate.clamped,
        "120 fps is outside the mode's frame lengths: {rate:?}"
    );
}

#[test]
fn a_mode_whose_shortest_frame_is_137_76_fps_gives_120_within_0_06_percent_by_lengthening_it() {
    // 1000 lines of 7.257556 us each: 137.760 fps at the shortest (the contract's rate).
    let m = mode(1_000, 7_257_556);
    let controls = NativeControls::new(&m, initial(120_000)).expect("controls");
    let rate = controls.initial_rate();
    assert!(!rate.clamped, "{rate:?}");
    assert!(rate.lines > 1_000, "the frame is lengthened: {rate:?}");
    assert_eq!(
        rate.achieved_milli,
        fps_milli_of(7_257_556, rate.lines).unwrap(),
        "the rate is the contract's for the frame it picked"
    );
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

#[test]
fn achieved_rate_is_the_contracts_rate_for_several_frame_lengths_including_the_shortest() {
    // Each frame length's rate is the contract's `fps_milli_of`, and the frame that rate asks for
    // runs at exactly that rate: the achieved rate is the contract's period for it.
    let line_time_ps = 8_710_801;
    let m = mode(1_000, line_time_ps);
    for lines in [1_000, 1_001, 1_500, 2_000, 10_000, 52_000] {
        let rate_of_lines = fps_milli_of(line_time_ps, lines).expect("a rate");
        // The contract's period gives the same rate: 1e15 over the period, rounded.
        let period = m.period_ps(lines);
        let from_period = u32::try_from((1_000_000_000_000_000 + period / 2) / period).unwrap();
        assert_eq!(rate_of_lines, from_period, "{lines} lines");
        let controls = NativeControls::new(&m, initial(rate_of_lines)).expect("controls");
        let rate = controls.initial_rate();
        // Rates are whole mfps, so at a long frame several lengths share one rate: the frame picked
        // can be another length with the same rate. The rate is what is asserted.
        assert_eq!(rate.achieved_milli, rate_of_lines, "{lines} lines");
        assert_eq!(
            fps_milli_of(line_time_ps, rate.lines),
            Some(rate.achieved_milli),
            "{lines} lines"
        );
        assert_eq!(
            frame_length_for_fps_milli(line_time_ps, 1, u32::MAX, rate_of_lines)
                .map(|(_, rate)| rate),
            Some(rate_of_lines),
        );
    }
}
