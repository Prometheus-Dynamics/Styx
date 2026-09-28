//! Sensor modes as capture modes: sizes, bus codes and exact frame interval ranges from the
//! description's timing.
//!
//! A frame lasts `line_length × frame_length / pixel_rate` seconds, so every interval the
//! sensor can produce is a fraction of integers: the ranges here are exact, not rounded fps.

use std::time::Duration;

use styx_graph::Fraction;
use styx_sensor::{SensorDescription, Timing};

/// One mode and bit depth of a sensor.
#[derive(Clone, Debug, PartialEq)]
pub struct SensorMode {
    /// Mode name in the description (`1280x800`).
    pub mode: String,
    /// Format name in the description (`raw10`).
    pub format: String,
    /// Media bus code.
    pub code: u32,
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Bits per sample.
    pub bits: u8,
    /// Shortest frame interval (fastest rate), exact.
    pub min_interval: Fraction,
    /// Longest frame interval (slowest rate), exact.
    pub max_interval: Fraction,
    /// The interval at the mode's default vertical blanking.
    pub default_interval: Fraction,
    /// Timing model of the mode.
    pub timing: Timing,
}

impl SensorMode {
    /// Fastest frame rate.
    pub fn max_fps(&self) -> f64 {
        self.min_interval.fps()
    }

    /// Slowest frame rate.
    pub fn min_fps(&self) -> f64 {
        self.max_interval.fps()
    }

    /// Whether a frame interval is within the mode's range.
    pub fn allows(&self, interval: Fraction) -> bool {
        self.min_interval <= interval && interval <= self.max_interval
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The exact duration of `frame_length` lines as a fraction of a second, reduced; falls back to
/// nanoseconds when the numbers do not fit 32 bits.
pub fn frame_interval(timing: &Timing, frame_length: u32) -> Fraction {
    let num = u64::from(timing.line_length()) * u64::from(frame_length);
    let den = timing.pixel_rate.max(1);
    let g = gcd(num, den).max(1);
    let (num, den) = (num / g, den / g);
    match (u32::try_from(num), u32::try_from(den)) {
        (Ok(n), Ok(d)) => Fraction::new(n, d),
        _ => {
            let ns = num as u128 * 1_000_000_000 / den as u128;
            let g = gcd(ns as u64, 1_000_000_000).max(1);
            Fraction::new(
                u32::try_from(ns as u64 / g).unwrap_or(u32::MAX),
                (1_000_000_000 / g) as u32,
            )
        }
    }
}

/// Converts a frame interval to a duration.
pub fn interval_duration(interval: Fraction) -> Duration {
    if interval.den == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(
        (u128::from(interval.num) * 1_000_000_000 / u128::from(interval.den)) as u64,
    )
}

/// Every mode × format of a description, in description order.
pub fn sensor_modes(desc: &SensorDescription) -> Vec<SensorMode> {
    let mut out = Vec::new();
    for m in &desc.modes {
        for (fname, f) in desc.formats_of(m) {
            let timing = Timing::for_mode(m, f, &desc.controls.exposure);
            out.push(SensorMode {
                mode: m.name.clone(),
                format: fname.to_owned(),
                code: f.code.0,
                width: m.size.width,
                height: m.size.height,
                bits: f.bits(),
                min_interval: frame_interval(&timing, timing.frame_length_min()),
                max_interval: frame_interval(&timing, timing.frame_length_max()),
                default_interval: frame_interval(&timing, timing.frame_length_default()),
                timing,
            });
        }
    }
    out
}

/// The mode for a size and bus code (the first when several match).
pub fn find_mode(modes: &[SensorMode], width: u32, height: u32, code: u32) -> Option<&SensorMode> {
    modes
        .iter()
        .find(|m| m.width == width && m.height == height && m.code == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov9782() -> SensorDescription {
        SensorDescription::from_file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sensor/sensors/ov9782.toml"
        ))
        .unwrap()
    }

    #[test]
    fn intervals_are_exact_fractions_of_the_timing() {
        let modes = sensor_modes(&ov9782());
        // 3 modes x 2 bit depths.
        assert_eq!(modes.len(), 6);
        let m = find_mode(&modes, 1280, 800, 0x3007).unwrap();
        assert_eq!(
            (m.mode.as_str(), m.format.as_str(), m.bits),
            ("1280x800", "raw10", 10)
        );
        // 1456 x 910 / 160e6 and 1456 x 52340 / 160e6, reduced.
        assert_eq!(m.min_interval, Fraction::new(1456 * 910, 160_000_000));
        assert_eq!((m.min_interval.num, m.min_interval.den), (8281, 1_000_000));
        assert_eq!(m.max_interval, Fraction::new(1456 * 52340, 160_000_000));
        assert!((m.max_fps() - 120.758).abs() < 0.001, "{}", m.max_fps());
        assert!((m.min_fps() - 2.0995).abs() < 0.001, "{}", m.min_fps());
        assert!((m.default_interval.fps() - 60.313).abs() < 0.001);
        assert!(m.allows(Fraction::from_fps(30)));
        assert!(m.allows(Fraction::from_fps(120)));
        assert!(!m.allows(Fraction::from_fps(121)));
        assert_eq!(
            interval_duration(Fraction::from_fps(40)),
            Duration::from_millis(25)
        );
        assert!(find_mode(&modes, 640, 400, 0x3001).is_some());
        assert!(find_mode(&modes, 640, 480, 0x3007).is_none());
    }

    #[test]
    fn large_numbers_fall_back_to_nanoseconds() {
        let desc = ov9782();
        let mut t = desc.timing("1280x800", "raw10").unwrap();
        t.pixel_rate = 9_999_999_937; // prime-ish, > u32::MAX
        let f = frame_interval(&t, 1000);
        let expect = 1456.0 * 1000.0 / 9_999_999_937.0;
        assert!((1.0 / f.fps() - expect).abs() < 2e-9);
    }
}
