//! The gain conversion follows the mode's own gain law (the contract's `GainRange::law`): each
//! law's formula, and its refusals. The laws' round trips and monotonicity are in `src/gain.rs`.

mod common;

use lemnos_contract_camera_sensor::{GainLaw, GainRange};
use styx_native_controls::{Error, code_to_gain, gain_to_code};

const LINEAR: GainRange = GainRange {
    min: 16,
    max: 255,
    unity: 16,
    step: 1,
    law: GainLaw::Linear,
};

#[test]
fn linear_has_unity_at_the_unity_code() {
    assert_eq!(code_to_gain(&LINEAR, 16), Ok(1_000));
    assert_eq!(gain_to_code(&LINEAR, 1_000), Ok(16));
    assert_eq!(gain_to_code(&LINEAR, 2_000), Ok(32));
    assert_eq!(code_to_gain(&LINEAR, 32), Ok(2_000));
}

#[test]
fn reciprocal_codes_approach_full_as_the_gain_grows() {
    let range = GainRange {
        min: 0,
        max: 900,
        unity: 100,
        step: 1,
        law: GainLaw::Reciprocal { full: 1_000 },
    };
    assert_eq!(gain_to_code(&range, 2_000), Ok(550));
    assert_eq!(code_to_gain(&range, 550), Ok(2_000));
    assert!(code_to_gain(&range, 900).unwrap() > code_to_gain(&range, 550).unwrap());
    // A code at full has no gain: refused, not guessed.
    assert!(matches!(code_to_gain(&range, 1_000), Err(Error::Mode(_))));
}

#[test]
fn decibel_codes_are_steps_of_the_contract_s_milli_db() {
    let range = GainRange {
        min: 0,
        max: 100,
        unity: 10,
        step: 1,
        law: GainLaw::Decibel {
            milli_db_per_code: 1_000,
        },
    };
    assert_eq!(code_to_gain(&range, 16), Ok(1_995));
    assert_eq!(gain_to_code(&range, 10_000), Ok(30));
}

#[test]
fn a_mode_without_a_unity_code_or_a_zero_gain_is_refused_under_every_law() {
    let no_unity = GainRange { unity: 0, ..LINEAR };
    assert!(matches!(
        gain_to_code(&no_unity, 1_000),
        Err(Error::Mode(_))
    ));
    assert!(matches!(code_to_gain(&no_unity, 16), Err(Error::Mode(_))));
    assert!(matches!(gain_to_code(&LINEAR, 0), Err(Error::Mode(_))));
}
