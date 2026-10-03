//! Frame rates: whether a mode can run at the rate asked for, and the interval a capture runs at.
//!
//! A mode lists rates (USB cameras), allows any rate in a range (sensors Styx drives), or both.
//! Rates asked for match a listed rate within 1% (29.97 fps is 30); a range runs at exactly the
//! rate asked for.

use styx_core::prelude::*;

use super::DEFAULT_FPS;
use super::request::FrameRate;
use crate::prelude::Mode;

/// Relative tolerance matching a rate asked for against a listed one.
const TOLERANCE: f32 = 0.01;

/// The start of a rejection for a mode's rate (see [`PlanError::FrameRate`](super::PlanError)).
pub(crate) const REJECTED: &str = "cannot run at ";

fn near(fps: f32, want: u32) -> bool {
    (fps - want as f32).abs() <= want as f32 * TOLERANCE
}

/// `fps` is at least `min` and at most `max`, with the tolerance.
fn within(fps: f32, min: u32, max: u32) -> bool {
    fps >= min as f32 * (1.0 - TOLERANCE) && fps <= max as f32 * (1.0 + TOLERANCE)
}

/// The rates `mode` runs at, for messages: "30, 25, 15 fps", "any rate 2.1..120.6 fps" (a
/// range covers the rates a mode also lists).
pub(crate) fn describe(mode: &Mode) -> String {
    if let Some(range) = mode.interval_stepwise {
        return format!(
            "any rate {:.1}..{:.1} fps",
            range.max.fps(),
            range.min.fps()
        );
    }
    if mode.intervals.is_empty() {
        return "an unknown rate".into();
    }
    let list: Vec<String> = mode
        .intervals
        .iter()
        .map(|i| format!("{:.3}", i.fps()))
        .map(|s| s.trim_end_matches('0').trim_end_matches('.').to_string())
        .collect();
    format!("{} fps", list.join(", "))
}

/// Whether `mode` knows its rates at all (file sources, some virtual ones do not).
fn known(mode: &Mode) -> bool {
    !mode.intervals.is_empty() || mode.interval_stepwise.is_some()
}

/// The fastest interval of `mode` from `min` to `max` fps.
fn fastest_within(mode: &Mode, min: u32, max: u32) -> Option<Interval> {
    let listed = mode
        .intervals
        .iter()
        .copied()
        .filter(|i| within(i.fps(), min, max));
    let ranged = mode.interval_stepwise.and_then(|range| {
        // The range's fastest, or `max` where the range goes faster.
        let fastest = if range.min.fps() > max as f32 {
            Interval::from_fps(max)?
        } else {
            range.min
        };
        (fastest.within(range.min, range.max) && within(fastest.fps(), min, max)).then_some(fastest)
    });
    listed
        .chain(ranged)
        .max_by(|a, b| a.fps().total_cmp(&b.fps()))
}

/// Exactly `fps` on `mode`: the rate itself within a range, else the closest listed rate
/// within the tolerance.
fn exactly(mode: &Mode, fps: u32) -> Option<Interval> {
    let want = Interval::from_fps(fps)?;
    if mode.interval_stepwise.is_some_and(|s| s.contains(want)) {
        return Some(want);
    }
    mode.intervals
        .iter()
        .copied()
        .filter(|i| near(i.fps(), fps))
        .min_by(|a, b| {
            (a.fps() - fps as f32)
                .abs()
                .total_cmp(&(b.fps() - fps as f32).abs())
        })
}

/// The camera's default for `mode`: [`DEFAULT_FPS`] within its range, else the listed rate
/// closest to it (the faster of two as close).
pub(crate) fn default_interval(mode: &Mode) -> Option<Interval> {
    if let Some(range) = mode.interval_stepwise {
        let want = Interval::from_fps(DEFAULT_FPS)?;
        return Some(if want.fps() > range.min.fps() {
            range.min
        } else if want.fps() < range.max.fps() {
            range.max
        } else {
            want
        });
    }
    let distance = |i: &Interval| (i.fps() - DEFAULT_FPS as f32).abs();
    mode.intervals.iter().copied().min_by(|a, b| {
        distance(a)
            .total_cmp(&distance(b))
            .then(b.fps().total_cmp(&a.fps()))
    })
}

/// The interval `mode` runs at for `rate`; `None` when the mode does not say or cannot.
pub(crate) fn pick(mode: &Mode, rate: FrameRate) -> Option<Interval> {
    match rate {
        FrameRate::CameraDefault => default_interval(mode),
        FrameRate::Exactly(fps) => exactly(mode, fps),
        FrameRate::AtLeast(fps) => fastest_within(mode, fps, u32::MAX),
        FrameRate::Between(min, max) => fastest_within(mode, min, max),
    }
}

/// Whether `mode` can run at `rate`; the reason (starting with [`REJECTED`]) when it cannot.
/// Modes that do not know their rates pass.
pub(crate) fn check(mode: &Mode, rate: FrameRate) -> Result<(), String> {
    if rate == FrameRate::CameraDefault || !known(mode) || pick(mode, rate).is_some() {
        return Ok(());
    }
    let asked = match rate {
        FrameRate::Exactly(fps) => format!("{fps} fps"),
        FrameRate::AtLeast(fps) => format!("{fps} fps or faster"),
        FrameRate::Between(min, max) => format!("{min} to {max} fps"),
        FrameRate::CameraDefault => unreachable!("passes above"),
    };
    Err(format!("{REJECTED}{asked}: runs at {}", describe(mode)))
}

/// One rate for consumers sharing a capture: the rate every one of them accepts. An error when
/// there is none (two different exact rates, or bounds that exclude each other).
pub(crate) fn combine(rates: &[FrameRate]) -> Result<FrameRate, String> {
    let mut exact: Option<u32> = None;
    let mut min: Option<u32> = None;
    let mut max: Option<u32> = None;
    for &rate in rates {
        match rate {
            FrameRate::CameraDefault => {}
            FrameRate::Exactly(fps) => match exact {
                Some(other) if other != fps => {
                    return Err(format!(
                        "consumers of one capture ask for exactly {other} and {fps} fps"
                    ));
                }
                _ => exact = Some(fps),
            },
            FrameRate::AtLeast(fps) => min = Some(min.map_or(fps, |m| m.max(fps))),
            FrameRate::Between(lo, hi) => {
                min = Some(min.map_or(lo, |m| m.max(lo)));
                max = Some(max.map_or(hi, |m| m.min(hi)));
            }
        }
    }
    let lo = min.unwrap_or(0);
    let hi = max.unwrap_or(u32::MAX);
    if lo > hi {
        return Err(format!(
            "consumers of one capture ask for at least {lo} and at most {hi} fps"
        ));
    }
    Ok(match (exact, min, max) {
        (Some(fps), ..) if fps < lo || fps > hi => {
            return Err(format!(
                "a consumer asks for exactly {fps} fps, another for {}",
                if hi == u32::MAX {
                    format!("at least {lo} fps")
                } else {
                    format!("{lo} to {hi} fps")
                }
            ));
        }
        (Some(fps), ..) => FrameRate::Exactly(fps),
        (None, None, None) => FrameRate::CameraDefault,
        (None, Some(lo), None) => FrameRate::AtLeast(lo),
        (None, ..) => FrameRate::Between(lo, hi),
    })
}

#[cfg(test)]
mod tests {
    use smallvec::smallvec;
    use styx_capture::ModeId;

    use super::*;

    fn listed(rates: &[u32]) -> Mode {
        let format =
            MediaFormat::with_default_color(FourCc::MJPG, Resolution::new(640, 480).unwrap());
        Mode {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: rates
                .iter()
                .map(|&r| Interval::from_fps(r).unwrap())
                .collect(),
            interval_stepwise: None,
        }
    }

    fn ranged(slowest: u32, fastest: u32) -> Mode {
        let mut mode = listed(&[]);
        mode.intervals = smallvec![];
        mode.interval_stepwise = Some(IntervalStepwise {
            min: Interval::from_fps(fastest).unwrap(),
            max: Interval::from_fps(slowest).unwrap(),
            step: Interval::new(1000, 1).unwrap(),
        });
        mode
    }

    fn fps(interval: Option<Interval>) -> Option<f32> {
        interval.map(|i| (i.fps() * 100.0).round() / 100.0)
    }

    #[test]
    fn exact_rates_on_lists_and_ranges() {
        let usb = listed(&[30, 25, 15, 5]);
        assert_eq!(fps(pick(&usb, FrameRate::Exactly(15))), Some(15.0));
        // 29.97 fps counts as 30.
        let ntsc = Mode {
            intervals: smallvec![Interval::new(1001, 30000).unwrap()],
            ..listed(&[])
        };
        assert_eq!(fps(pick(&ntsc, FrameRate::Exactly(30))), Some(29.97));
        let err = check(&usb, FrameRate::Exactly(60)).unwrap_err();
        assert_eq!(err, "cannot run at 60 fps: runs at 30, 25, 15, 5 fps");
        let sensor = ranged(2, 120);
        assert_eq!(fps(pick(&sensor, FrameRate::Exactly(45))), Some(45.0));
        assert!(
            check(&sensor, FrameRate::Exactly(200))
                .unwrap_err()
                .contains("any rate 2.0..120.0")
        );
    }

    #[test]
    fn at_least_and_between_take_the_fastest_allowed() {
        let usb = listed(&[30, 25, 15]);
        assert_eq!(fps(pick(&usb, FrameRate::AtLeast(20))), Some(30.0));
        assert_eq!(fps(pick(&usb, FrameRate::Between(10, 26))), Some(25.0));
        assert!(check(&usb, FrameRate::AtLeast(31)).is_err());
        assert!(check(&usb, FrameRate::Between(16, 24)).is_err());
        let sensor = ranged(2, 120);
        assert_eq!(fps(pick(&sensor, FrameRate::AtLeast(30))), Some(120.0));
        assert_eq!(fps(pick(&sensor, FrameRate::Between(15, 60))), Some(60.0));
        assert!(check(&sensor, FrameRate::Between(150, 200)).is_err());
    }

    #[test]
    fn default_is_30_or_the_listed_rate_closest_to_it() {
        assert_eq!(fps(default_interval(&ranged(2, 120))), Some(30.0));
        assert_eq!(fps(default_interval(&ranged(2, 20))), Some(20.0));
        assert_eq!(fps(default_interval(&ranged(60, 120))), Some(60.0));
        assert_eq!(fps(default_interval(&listed(&[60, 30, 15]))), Some(30.0));
        assert_eq!(fps(default_interval(&listed(&[60, 15]))), Some(15.0));
        assert_eq!(fps(default_interval(&listed(&[25, 35]))), Some(35.0));
        assert_eq!(default_interval(&listed(&[])), None);
    }

    #[test]
    fn consumers_rates_combine() {
        use FrameRate::*;
        assert_eq!(combine(&[CameraDefault, CameraDefault]), Ok(CameraDefault));
        assert_eq!(combine(&[Exactly(30), CameraDefault]), Ok(Exactly(30)));
        assert_eq!(combine(&[Exactly(30), AtLeast(15)]), Ok(Exactly(30)));
        assert_eq!(combine(&[AtLeast(15), AtLeast(25)]), Ok(AtLeast(25)));
        assert_eq!(
            combine(&[AtLeast(15), Between(10, 60)]),
            Ok(Between(15, 60))
        );
        assert!(combine(&[Exactly(30), Exactly(60)]).is_err());
        assert!(combine(&[Exactly(30), AtLeast(60)]).is_err());
        assert!(combine(&[AtLeast(30), Between(10, 20)]).is_err());
    }
}
