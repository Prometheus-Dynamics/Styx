//! The gain model: ratios to register codes with quantisation, and analogue/digital splitting.

use crate::desc::{Gain, GainModel};
#[cfg(not(feature = "std"))]
use crate::math::Float as _;

/// How to quantise a gain that falls between codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// The code whose gain is closest.
    Nearest,
    /// The highest code whose gain does not exceed the request.
    Down,
}

/// A quantised gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainCode {
    /// Register code.
    pub code: u32,
    /// The gain this code gives.
    pub gain: f64,
    /// The request was outside the code range and was clamped.
    pub clamped: bool,
}

/// A total gain split into analogue then digital gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainSplit {
    /// Analogue part.
    pub analog: GainCode,
    /// Digital part, when the sensor has digital gain.
    pub digital: Option<GainCode>,
    /// `analog × digital`, the gain actually obtained.
    pub total: f64,
}

const EPS: f64 = 1e-9;

impl Gain {
    /// The gain of a code (codes outside the range are evaluated as given).
    pub fn gain_for_code(&self, code: u32) -> f64 {
        match &self.model {
            GainModel::Linear { step, offset } => (i64::from(code) + offset) as f64 * step,
            GainModel::Reciprocal { numerator, base } => numerator / (base - f64::from(code)),
            GainModel::Table(t) => match t.iter().find(|(c, _)| *c == code) {
                Some((_, g)) => *g,
                // Between table entries: the highest entry not above the code.
                None => t
                    .iter()
                    .rev()
                    .find(|(c, _)| *c <= code)
                    .or(t.first())
                    .map_or(1.0, |e| e.1),
            },
        }
    }

    /// Lowest and highest gain.
    pub fn range(&self) -> (f64, f64) {
        match &self.model {
            GainModel::Table(t) => (
                t.first().map_or(1.0, |e| e.1),
                t.last().map_or(1.0, |e| e.1),
            ),
            _ => (
                self.gain_for_code(self.min_code),
                self.gain_for_code(self.max_code),
            ),
        }
    }

    /// The code for a gain.
    pub fn code_for_gain(&self, gain: f64, rounding: Rounding) -> GainCode {
        let (lo, hi) = self.range();
        let clamped = gain < lo - EPS || gain > hi + EPS;
        // Candidates: every table code, or the two codes around the model's exact value (no
        // allocation: this runs for every gain request).
        let (table, (pair, n)): (&[(u32, f64)], _) = match &self.model {
            GainModel::Table(t) => (t, ([0, 0], 0)),
            GainModel::Linear { step, offset } => (&[], bracket(gain / step - *offset as f64)),
            GainModel::Reciprocal { numerator, base } => {
                if gain <= 0.0 {
                    (&[], ([self.min_code, 0], 1))
                } else {
                    (&[], bracket(base - numerator / gain))
                }
            }
        };
        let codes = table
            .iter()
            .map(|(c, _)| *c)
            .chain(pair.into_iter().take(n))
            .map(|c| c.clamp(self.min_code, self.max_code));
        let best = match rounding {
            Rounding::Nearest => codes.min_by(|a, b| {
                let da = (self.gain_for_code(*a) - gain).abs();
                let db = (self.gain_for_code(*b) - gain).abs();
                da.total_cmp(&db).then(a.cmp(b))
            }),
            Rounding::Down => codes
                .filter(|c| self.gain_for_code(*c) <= gain + EPS)
                .max_by(|a, b| self.gain_for_code(*a).total_cmp(&self.gain_for_code(*b))),
        };
        let code = best.unwrap_or(self.min_code);
        GainCode {
            code,
            gain: self.gain_for_code(code),
            clamped,
        }
    }
}

/// The codes around `x` (and how many).
fn bracket(x: f64) -> ([u32; 2], usize) {
    if !x.is_finite() {
        return ([u32::MAX, 0], 1);
    }
    let f = x.floor().clamp(0.0, f64::from(u32::MAX)) as u32;
    ([f, f.saturating_add(1)], 2)
}

/// Split a total gain into analogue gain first, then digital gain for the remainder.
///
/// With digital gain the analogue part is rounded down so the digital part (≥ 1) makes up the
/// rest; without it the analogue gain is the nearest code.
pub fn split_gain(analog: &Gain, digital: Option<&Gain>, total: f64) -> GainSplit {
    match digital {
        None => {
            let a = analog.code_for_gain(total, Rounding::Nearest);
            GainSplit {
                analog: a,
                digital: None,
                total: a.gain,
            }
        }
        Some(dg) => {
            let mut a = analog.code_for_gain(total, Rounding::Down);
            let d = dg.code_for_gain(total / a.gain, Rounding::Nearest);
            let (_, dmax) = dg.range();
            let (amin, amax) = analog.range();
            a.clamped = total < amin - EPS || total > amax * dmax + EPS;
            GainSplit {
                analog: a,
                digital: Some(d),
                total: a.gain * d.gain,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear(min: u32, max: u32, step: f64) -> Gain {
        Gain {
            register: None,
            min_code: min,
            max_code: max,
            default_code: min,
            model: GainModel::Linear { step, offset: 0 },
        }
    }

    #[test]
    fn linear_codes() {
        let g = linear(0x10, 0xff, 1.0 / 16.0);
        assert_eq!(g.range(), (1.0, 15.9375));
        assert_eq!(g.code_for_gain(2.0, Rounding::Nearest).code, 0x20);
        let c = g.code_for_gain(2.03, Rounding::Nearest);
        assert_eq!((c.code, c.gain, c.clamped), (0x20, 2.0, false));
        assert_eq!(g.code_for_gain(2.05, Rounding::Nearest).code, 0x21);
        assert_eq!(g.code_for_gain(2.05, Rounding::Down).code, 0x20);
        let c = g.code_for_gain(100.0, Rounding::Nearest);
        assert_eq!((c.code, c.clamped), (0xff, true));
        let c = g.code_for_gain(0.5, Rounding::Down);
        assert_eq!((c.code, c.clamped), (0x10, true));
    }

    #[test]
    fn reciprocal_codes() {
        // IMX219-style: gain = 256 / (256 - code), codes 0..=232.
        let g = Gain {
            register: None,
            min_code: 0,
            max_code: 232,
            default_code: 0,
            model: GainModel::Reciprocal {
                numerator: 256.0,
                base: 256.0,
            },
        };
        assert_eq!(g.gain_for_code(128), 2.0);
        assert_eq!(g.code_for_gain(2.0, Rounding::Nearest).code, 128);
        assert_eq!(g.code_for_gain(4.0, Rounding::Nearest).code, 192);
        let c = g.code_for_gain(3.0, Rounding::Down);
        assert!(c.gain <= 3.0 && g.gain_for_code(c.code + 1) > 3.0);
        assert_eq!(g.range().1, 256.0 / 24.0);
    }

    #[test]
    fn table_codes() {
        let g = Gain {
            register: None,
            min_code: 0,
            max_code: 3,
            default_code: 0,
            model: GainModel::Table(vec![(0, 1.0), (1, 2.0), (3, 4.0)]),
        };
        assert_eq!(g.code_for_gain(2.9, Rounding::Nearest).code, 1);
        assert_eq!(g.code_for_gain(3.1, Rounding::Nearest).code, 3);
        assert_eq!(g.code_for_gain(3.9, Rounding::Down).code, 1);
        assert_eq!(g.gain_for_code(2), 2.0);
    }

    #[test]
    fn split_analog_then_digital() {
        let a = linear(0x10, 0xff, 1.0 / 16.0);
        let d = linear(0x100, 0xfff, 1.0 / 256.0);
        let s = split_gain(&a, Some(&d), 4.0);
        assert_eq!((s.analog.code, s.digital.unwrap().code), (0x40, 0x100));
        let s = split_gain(&a, Some(&d), 32.0);
        assert_eq!(s.analog.code, 0xff);
        assert!((s.total - 32.0).abs() < 0.01, "{}", s.total);
        assert!(!s.analog.clamped);
        let s = split_gain(&a, Some(&d), 4.03);
        assert_eq!(s.analog.code, 0x40);
        assert!((s.total - 4.03).abs() < 1.0 / 256.0 * 4.0);
        let s = split_gain(&a, None, 32.0);
        assert!(s.analog.clamped && s.digital.is_none() && s.total == 15.9375);
    }
}
