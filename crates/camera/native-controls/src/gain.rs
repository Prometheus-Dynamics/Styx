//! The gain law: how a gain in thousandths becomes the sensor's code, and back.
//!
//! The law is the contract's: [`GainRange::law`] on the mode's analogue gain (`camera.sensor/1`
//! `GainLaw`). Every gain conversion goes through [`gain_to_code`] and [`code_to_gain`], so the
//! three laws live in these two functions and nowhere else. The formulas are the contract's own
//! (its `GainLaw` doc comment), with `u` the unity code and `c` a code:
//!
//! - `Linear`: gain = c / u.
//! - `Reciprocal { full: N }`: gain = (N - u) / (N - c), so codes approach N as the gain grows.
//! - `Decibel { milli_db_per_code: s }`: gain = 10^((c - u) * s / 20000).
//!
//! A gain falls between two codes, so a code is the nearer one, and the codes are then moved in the
//! range's `step`s from its `min`.

use lemnos_contract_camera_sensor::{GainLaw, GainRange};

use crate::error::Error;

/// The sensor's gain code for `milli` thousandths of gain under the range's law, rounded to the
/// nearest code and then to the nearest step counted from the lowest code. A gain the law gives no
/// code for (zero, or a gain past a reciprocal law's full code) is refused. Does not check the
/// range's bounds: the caller does.
pub fn gain_to_code(range: &GainRange, milli: u32) -> Result<u32, Error> {
    if range.unity == 0 {
        return Err(Error::Mode("analogue gain has no unity code"));
    }
    if milli == 0 {
        return Err(Error::Mode("a gain of zero has no code"));
    }
    let raw: f64 = match range.law {
        GainLaw::Linear => f64::from(milli) * f64::from(range.unity) / 1000.0,
        GainLaw::Reciprocal { full } => {
            if range.max >= full {
                return Err(Error::Mode(
                    "reciprocal gain law: the full code is not above the gain codes",
                ));
            }
            let (full, unity, gain) = (
                f64::from(full),
                f64::from(range.unity),
                f64::from(milli) / 1000.0,
            );
            full - (full - unity) / gain
        }
        GainLaw::Decibel { milli_db_per_code } => {
            let (unity, step) = (f64::from(range.unity), f64::from(milli_db_per_code));
            unity + 20_000.0 * (f64::from(milli) / 1000.0).log10() / step
        }
    };
    if !raw.is_finite() {
        return Err(Error::Mode("gain has no code under the gain law"));
    }
    Ok(step_code(raw.round().max(0.0) as u64, range))
}

/// Thousandths of gain for a sensor's gain code under the range's law, rounded to the nearest
/// thousandth. The inverse of [`gain_to_code`]. A code the law does not give a gain for (a
/// reciprocal law's full code or past it) is refused.
pub fn code_to_gain(range: &GainRange, code: u32) -> Result<u32, Error> {
    if range.unity == 0 {
        return Err(Error::Mode("analogue gain has no unity code"));
    }
    let milli: f64 = match range.law {
        GainLaw::Linear => f64::from(code) * 1000.0 / f64::from(range.unity),
        GainLaw::Reciprocal { full } => {
            if code >= full {
                return Err(Error::Mode(
                    "reciprocal gain law: a code at or past the full code has no gain",
                ));
            }
            let (full, unity, code) = (f64::from(full), f64::from(range.unity), f64::from(code));
            1000.0 * (full - unity) / (full - code)
        }
        GainLaw::Decibel { milli_db_per_code } => {
            let (unity, step) = (f64::from(range.unity), f64::from(milli_db_per_code));
            1000.0 * 10f64.powf((f64::from(code) - unity) * step / 20_000.0)
        }
    };
    Ok(milli.round().clamp(0.0, f64::from(u32::MAX)) as u32)
}

/// Rounds a raw code to the nearest step counted from the range's lowest code.
fn step_code(raw: u64, range: &GainRange) -> u32 {
    let min = u64::from(range.min);
    let step = u64::from(range.step.max(1));
    let code = if raw <= min {
        raw
    } else {
        let rel = raw - min;
        min + (rel + step / 2) / step * step
    };
    u32::try_from(code).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR: GainRange = GainRange {
        min: 16,
        max: 255,
        unity: 16,
        step: 1,
        law: GainLaw::Linear,
    };

    #[test]
    fn linear_is_code_over_unity() {
        assert_eq!(gain_to_code(&LINEAR, 1_000), Ok(16));
        assert_eq!(gain_to_code(&LINEAR, 2_000), Ok(32));
        assert_eq!(code_to_gain(&LINEAR, 16), Ok(1_000));
        assert_eq!(code_to_gain(&LINEAR, 40), Ok(2_500));
    }

    #[test]
    fn steps_round_from_the_lowest_code() {
        let g = GainRange {
            min: 8,
            max: 200,
            unity: 8,
            step: 4,
            law: GainLaw::Linear,
        };
        // Raw code 14 is 6 above the lowest code 8; 6 rounds to the step 8 (4 * 2), giving 16.
        assert_eq!(gain_to_code(&g, 1_750), Ok(8 + (6 + 2) / 4 * 4));
    }

    fn reciprocal() -> GainRange {
        GainRange {
            min: 0,
            max: 900,
            unity: 0,
            step: 1,
            law: GainLaw::Reciprocal { full: 1000 },
        }
    }

    #[test]
    fn reciprocal_gain_is_full_less_unity_over_full_less_code() {
        let g = GainRange {
            unity: 100,
            ..reciprocal()
        };
        // Unity is the code 100. A gain of two needs 1000 - 900 / 2 = 550.
        assert_eq!(code_to_gain(&g, 100), Ok(1_000));
        assert_eq!(code_to_gain(&g, 550), Ok(2_000));
        assert_eq!(gain_to_code(&g, 2_000), Ok(550));
        // Eight: 1000 - 900 / 8 = 887.5, rounded to 888 (nearest code).
        assert_eq!(gain_to_code(&g, 8_000), Ok(888));
    }

    #[test]
    fn decibel_gain_is_ten_to_the_code_difference_times_the_step() {
        // Unity at code 10. Each code is 1000 milli-dB: six codes is 6 dB, a gain of 1.9953.
        let g = GainRange {
            min: 0,
            max: 100,
            unity: 10,
            step: 1,
            law: GainLaw::Decibel {
                milli_db_per_code: 1_000,
            },
        };
        assert_eq!(code_to_gain(&g, 10), Ok(1_000));
        assert_eq!(code_to_gain(&g, 16), Ok(1_995));
        assert_eq!(code_to_gain(&g, 30), Ok(10_000));
        assert_eq!(gain_to_code(&g, 10_000), Ok(30));
    }

    /// Linear and decibel round trips are exact. A reciprocal code can only come back within one
    /// code: a thousandth of gain is worth up to (N - c)^2 / (N - u) / 1000 codes, under one code
    /// across the range, so the gain's own rounding decides the last code.
    #[test]
    fn every_law_round_trips_code_to_gain_to_code() {
        let laws = [
            LINEAR,
            GainRange {
                unity: 100,
                ..reciprocal()
            },
            GainRange {
                min: 0,
                max: 200,
                unity: 40,
                step: 1,
                law: GainLaw::Decibel {
                    milli_db_per_code: 300,
                },
            },
        ];
        for range in laws {
            for code in range.min..=range.max {
                let Ok(milli) = code_to_gain(&range, code) else {
                    continue;
                };
                if milli == 0 {
                    continue;
                }
                let back = gain_to_code(&range, milli).expect("gain has a code");
                let slack = match range.law {
                    GainLaw::Linear => 0,
                    _ => 1,
                };
                assert!(
                    back.abs_diff(code) <= slack,
                    "{:?} code {code} gain {milli} back {back}",
                    range.law
                );
            }
        }
    }

    #[test]
    fn every_law_is_monotonic_in_the_code_and_the_gain() {
        let laws = [
            LINEAR,
            GainRange {
                unity: 100,
                ..reciprocal()
            },
            GainRange {
                min: 0,
                max: 200,
                unity: 40,
                step: 1,
                law: GainLaw::Decibel {
                    milli_db_per_code: 300,
                },
            },
        ];
        for range in laws {
            let mut last = 0;
            for code in range.min..=range.max {
                let Ok(milli) = code_to_gain(&range, code) else {
                    continue;
                };
                assert!(
                    milli >= last,
                    "{:?}: code {code} gives {milli} after {last}",
                    range.law
                );
                last = milli;
            }
            let mut last = 0;
            for milli in (1_000..=40_000).step_by(250) {
                let Ok(code) = gain_to_code(&range, milli) else {
                    continue;
                };
                assert!(
                    code >= last,
                    "{:?}: gain {milli} gives {code} after {last}",
                    range.law
                );
                last = code;
            }
        }
    }

    #[test]
    fn no_unity_code_is_refused_and_a_gain_with_no_code_is_refused() {
        let no_unity = GainRange { unity: 0, ..LINEAR };
        assert!(matches!(
            gain_to_code(&no_unity, 1_000),
            Err(Error::Mode(_))
        ));
        assert!(matches!(code_to_gain(&no_unity, 16), Err(Error::Mode(_))));
        assert!(matches!(gain_to_code(&LINEAR, 0), Err(Error::Mode(_))));
        // A reciprocal full code must lie above every gain code.
        let full_too_low = GainRange {
            max: 1_000,
            ..reciprocal()
        };
        assert!(matches!(
            gain_to_code(&full_too_low, 2_000),
            Err(Error::Mode(_))
        ));
        assert!(matches!(
            code_to_gain(&reciprocal(), 1_000),
            Err(Error::Mode(_))
        ));
    }
}
