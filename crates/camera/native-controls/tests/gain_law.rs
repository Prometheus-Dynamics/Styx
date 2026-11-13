//! The gain law is a caller's setting, never assumed: `Unknown` is refused, `Linear` converts.

mod common;

use common::*;
use lemnos_contract_camera_sensor::GainRange;
use styx_native_controls::{Error, GainLaw, NativeControls, code_to_gain, gain_to_code};

const RANGE: GainRange = GainRange {
    min: 16,
    max: 255,
    unity: 16,
    step: 1,
};

#[test]
fn unknown_gain_law_refuses_the_controls_and_every_conversion() {
    let err = NativeControls::new(&mode(0, 1, 0), START, GainLaw::Unknown).unwrap_err();
    assert!(matches!(err, Error::GainLaw), "got {err:?}");
    assert_eq!(
        err.to_string(),
        "gain law unknown: Lemnos J4 adds it to the contract; set GainLaw::Linear only for a sensor known to be linear"
    );
    assert!(matches!(
        gain_to_code(GainLaw::Unknown, &RANGE, 2_000),
        Err(Error::GainLaw)
    ));
    assert!(matches!(
        code_to_gain(GainLaw::Unknown, &RANGE, 32),
        Err(Error::GainLaw)
    ));
}

#[test]
fn linear_gain_law_converts_both_ways_and_the_default_is_unknown() {
    assert_eq!(GainLaw::default(), GainLaw::Unknown);
    assert_eq!(gain_to_code(GainLaw::Linear, &RANGE, 2_000).unwrap(), 32);
    assert_eq!(code_to_gain(GainLaw::Linear, &RANGE, 32).unwrap(), 2_000);
}

#[test]
fn a_linear_range_without_unity_is_refused() {
    let no_unity = GainRange { unity: 0, ..RANGE };
    assert!(matches!(
        gain_to_code(GainLaw::Linear, &no_unity, 1_000),
        Err(Error::Mode(_))
    ));
}
