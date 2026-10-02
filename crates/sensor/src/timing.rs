//! The timing model: `fps = pixel_rate / (line_length × frame_length)`.
//!
//! `line_length = width + hblank` (pixels) and `frame_length = height + vblank` (lines). Exposure
//! is counted in lines and may be at most `frame_length - margin`.

use std::time::Duration;

use crate::desc::{Blanking, Exposure, Format, Mode};

/// Timing of one mode and format at a chosen horizontal blanking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timing {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Pixels per second.
    pub pixel_rate: u64,
    /// Horizontal blanking range (pixels).
    pub hblank_range: Blanking,
    /// Vertical blanking range (lines).
    pub vblank_range: Blanking,
    /// Current horizontal blanking.
    pub hblank: u32,
    /// Exposure limits and quantisation.
    pub exposure: ExposureSpec,
    /// Lines the sensor reads out beyond the frame length it is given: a frame of frame
    /// length `n` lasts `n + extra_lines` lines.
    pub extra_lines: u32,
}

/// Exposure limits and quantisation from the description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExposureSpec {
    /// Minimum lines.
    pub min: u32,
    /// `max = frame_length - margin`.
    pub margin: u32,
    /// Step in lines.
    pub step: u32,
    /// Fractional line bits (0: whole lines only).
    pub fraction_bits: u8,
}

impl From<&Exposure> for ExposureSpec {
    fn from(e: &Exposure) -> Self {
        Self {
            min: e.min,
            margin: e.margin,
            step: e.step.max(1),
            fraction_bits: e.fraction_bits,
        }
    }
}

/// A frame length choice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameLength {
    /// Frame length in lines (VTS).
    pub lines: u32,
    /// Vertical blanking (`lines - height`).
    pub vblank: u32,
    /// Resulting frame rate.
    pub fps: f64,
    /// Resulting frame duration.
    pub duration: Duration,
    /// The request was outside the mode's limits and was clamped.
    pub clamped: bool,
}

/// Exposure limits at a frame length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExposureLimits {
    /// Minimum in lines.
    pub min_lines: f64,
    /// Maximum in lines.
    pub max_lines: f64,
    /// Minimum duration.
    pub min: Duration,
    /// Maximum duration.
    pub max: Duration,
}

/// A quantised exposure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExposureValue {
    /// Lines, including any fractional part the sensor supports.
    pub lines: f64,
    /// Register code: `lines × 2^fraction_bits`.
    pub code: u32,
    /// The exposure time actually obtained.
    pub duration: Duration,
    /// The request was outside the limits and was clamped.
    pub clamped: bool,
}

impl Timing {
    /// Timing for a mode and format at the mode's default horizontal blanking.
    pub fn for_mode(mode: &Mode, format: &Format, exposure: &Exposure) -> Self {
        Self {
            width: mode.size.width,
            height: mode.size.height,
            pixel_rate: mode.pixel_rate.unwrap_or(format.pixel_rate),
            hblank_range: mode.hblank,
            vblank_range: mode.vblank,
            hblank: mode.hblank.default,
            exposure: exposure.into(),
            extra_lines: 0,
        }
    }

    /// The same timing for a sensor whose frames last `extra` lines longer than the frame
    /// length it is given.
    pub fn with_extra_lines(mut self, extra: u32) -> Self {
        self.extra_lines = extra;
        self
    }

    /// Lines a frame of frame length `frame_length` lasts.
    fn period_lines(&self, frame_length: u32) -> f64 {
        f64::from(frame_length) + f64::from(self.extra_lines)
    }

    /// The same timing with another horizontal blanking (clamped to the mode's range).
    pub fn with_hblank(mut self, hblank: u32) -> Self {
        self.hblank = hblank.clamp(self.hblank_range.min, self.hblank_range.max);
        self
    }

    /// Line length in pixels.
    pub fn line_length(&self) -> u32 {
        self.width + self.hblank
    }

    /// Time per line.
    pub fn line_time(&self) -> Duration {
        Duration::from_secs_f64(self.line_secs())
    }

    fn line_secs(&self) -> f64 {
        f64::from(self.line_length()) / self.pixel_rate as f64
    }

    /// Shortest frame length in lines.
    pub fn frame_length_min(&self) -> u32 {
        self.height + self.vblank_range.min
    }

    /// Longest frame length in lines.
    pub fn frame_length_max(&self) -> u32 {
        self.height + self.vblank_range.max
    }

    /// Frame length at the mode's default vertical blanking.
    pub fn frame_length_default(&self) -> u32 {
        self.height + self.vblank_range.default
    }

    /// Frame rate at a frame length.
    pub fn fps(&self, frame_length: u32) -> f64 {
        self.pixel_rate as f64 / (f64::from(self.line_length()) * self.period_lines(frame_length))
    }

    /// Frame duration at a frame length.
    pub fn frame_duration(&self, frame_length: u32) -> Duration {
        Duration::from_secs_f64(self.line_secs() * self.period_lines(frame_length))
    }

    /// Lowest and highest achievable frame rate at the current horizontal blanking.
    pub fn fps_range(&self) -> (f64, f64) {
        (
            self.fps(self.frame_length_max()),
            self.fps(self.frame_length_min()),
        )
    }

    fn frame_length(&self, lines: f64) -> FrameLength {
        let lo = self.frame_length_min();
        let hi = self.frame_length_max();
        let rounded = if lines.is_finite() {
            lines.round().max(0.0).min(f64::from(u32::MAX)) as u32
        } else {
            hi
        };
        let chosen = rounded.clamp(lo, hi);
        self.describe(chosen, chosen != rounded)
    }

    fn describe(&self, lines: u32, clamped: bool) -> FrameLength {
        FrameLength {
            lines,
            vblank: lines - self.height,
            fps: self.fps(lines),
            duration: self.frame_duration(lines),
            clamped,
        }
    }

    /// The frame length closest to a target frame rate, and the rate actually obtained.
    pub fn frame_length_for_fps(&self, fps: f64) -> FrameLength {
        self.frame_length(
            self.pixel_rate as f64 / (f64::from(self.line_length()) * fps)
                - f64::from(self.extra_lines),
        )
    }

    /// The frame length closest to a target frame duration.
    pub fn frame_length_for_duration(&self, duration: Duration) -> FrameLength {
        self.frame_length(duration.as_secs_f64() / self.line_secs() - f64::from(self.extra_lines))
    }

    /// Frame length limits for a frame rate range, e.g. 15–30 fps for low light: the shortest
    /// frame never exceeds `max_fps` and the longest never drops below `min_fps` (both clamped
    /// to what the mode allows). Returns `(shortest, longest)`.
    pub fn frame_length_range(&self, min_fps: f64, max_fps: f64) -> (FrameLength, FrameLength) {
        let (min_fps, max_fps) = if min_fps <= max_fps {
            (min_fps, max_fps)
        } else {
            (max_fps, min_fps)
        };
        let per_fps = self.pixel_rate as f64 / f64::from(self.line_length());
        let lo = self.frame_length_min();
        let hi = self.frame_length_max();
        let pick = |exact: f64, up: bool| -> FrameLength {
            let lines = if !exact.is_finite() {
                hi
            } else if up {
                exact.ceil().min(f64::from(u32::MAX)) as u32
            } else {
                exact.floor().max(0.0).min(f64::from(u32::MAX)) as u32
            };
            let chosen = lines.clamp(lo, hi);
            self.describe(chosen, chosen != lines)
        };
        let extra = f64::from(self.extra_lines);
        let shortest = pick(per_fps / max_fps - extra, true);
        let longest = pick(per_fps / min_fps - extra, false);
        if longest.lines < shortest.lines {
            // The range is narrower than one line of frame length: use the nearest single value.
            let one = self.frame_length_for_fps((min_fps + max_fps) / 2.0);
            return (one, one);
        }
        (shortest, longest)
    }

    /// Lines to duration.
    pub fn lines_to_duration(&self, lines: f64) -> Duration {
        Duration::from_secs_f64((lines * self.line_secs()).max(0.0))
    }

    /// Duration to (fractional) lines.
    pub fn duration_to_lines(&self, duration: Duration) -> f64 {
        duration.as_secs_f64() / self.line_secs()
    }

    /// Exposure limits at a frame length.
    pub fn exposure_limits(&self, frame_length: u32) -> ExposureLimits {
        let min_lines = f64::from(self.exposure.min);
        let max_lines = f64::from(frame_length.saturating_sub(self.exposure.margin)).max(min_lines);
        ExposureLimits {
            min_lines,
            max_lines,
            min: self.lines_to_duration(min_lines),
            max: self.lines_to_duration(max_lines),
        }
    }

    /// The exposure closest to `duration` that the sensor can do at `frame_length`.
    pub fn exposure(&self, duration: Duration, frame_length: u32) -> ExposureValue {
        let limits = self.exposure_limits(frame_length);
        let requested = self.duration_to_lines(duration);
        let clamped_lines = requested.clamp(limits.min_lines, limits.max_lines);
        let scale = f64::from(1u32 << self.exposure.fraction_bits);
        let lines = if self.exposure.fraction_bits > 0 {
            (clamped_lines * scale).round() / scale
        } else {
            let step = f64::from(self.exposure.step);
            let min = limits.min_lines;
            let n = ((clamped_lines - min) / step).round();
            // Stay within the maximum after rounding to the step.
            let mut l = min + n * step;
            if l > limits.max_lines {
                l -= step;
            }
            l.max(min)
        };
        ExposureValue {
            lines,
            code: (lines * scale).round() as u32,
            duration: self.lines_to_duration(lines),
            clamped: requested < limits.min_lines - 1e-9 || requested > limits.max_lines + 1e-9,
        }
    }

    /// Lines for an exposure register code.
    pub fn exposure_code_to_lines(&self, code: u32) -> f64 {
        f64::from(code) / f64::from(1u32 << self.exposure.fraction_bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> Timing {
        Timing {
            width: 1280,
            height: 800,
            pixel_rate: 160_000_000,
            hblank_range: Blanking {
                min: 176,
                max: 31487,
                default: 176,
            },
            vblank_range: Blanking {
                min: 110,
                max: 51540,
                default: 1022,
            },
            hblank: 176,
            extra_lines: 0,
            exposure: ExposureSpec {
                min: 1,
                margin: 12,
                step: 1,
                fraction_bits: 0,
            },
        }
    }

    #[test]
    fn fps_and_frame_length() {
        let t = timing();
        assert_eq!(t.line_length(), 1456);
        assert_eq!(t.line_time(), Duration::from_nanos(9100));
        let (lo, hi) = t.fps_range();
        assert!((hi - 160e6 / (1456.0 * 910.0)).abs() < 1e-9);
        assert!((lo - 160e6 / (1456.0 * 52340.0)).abs() < 1e-9);
        let f = t.frame_length_for_fps(30.0);
        // 160e6 / (1456 * 30) = 3663.00..
        assert_eq!(f.lines, 3663);
        assert_eq!(f.vblank, 2863);
        assert!((f.fps - 30.0).abs() < 0.01);
        assert!(!f.clamped);
        let f = t.frame_length_for_fps(1000.0);
        assert!(f.clamped);
        assert_eq!(f.lines, 910);
        let f = t.frame_length_for_duration(Duration::from_millis(50));
        assert_eq!(f.lines, 5495);
    }

    #[test]
    fn fps_range_to_frame_lengths() {
        let t = timing();
        let (short, long) = t.frame_length_range(15.0, 30.0);
        assert!(short.fps <= 30.0 && long.fps >= 15.0);
        // 3663 lines would give 30.00003 fps, just over the maximum.
        assert_eq!(short.lines, 3664);
        assert_eq!(long.lines, 7326);
        let (short, long) = t.frame_length_range(1.0, 500.0);
        assert!(short.clamped && long.clamped);
        assert_eq!((short.lines, long.lines), (910, 52340));
        // Reversed arguments are accepted.
        assert_eq!(t.frame_length_range(30.0, 15.0).0.lines, 3664);
    }

    #[test]
    fn exposure_limits_and_quantisation() {
        let t = timing();
        let l = t.exposure_limits(1822);
        assert_eq!(l.max_lines, 1810.0);
        assert_eq!(l.min, Duration::from_nanos(9100));
        let e = t.exposure(Duration::from_millis(10), 1822);
        assert_eq!(e.lines, 1099.0);
        assert_eq!(e.code, 1099);
        assert!(!e.clamped);
        let e = t.exposure(Duration::from_secs(1), 1822);
        assert!(e.clamped);
        assert_eq!(e.lines, 1810.0);
        let e = t.exposure(Duration::ZERO, 1822);
        assert!(e.clamped);
        assert_eq!(e.lines, 1.0);
    }

    #[test]
    fn fractional_exposure_and_steps() {
        let mut t = timing();
        t.exposure.fraction_bits = 4;
        let e = t.exposure(Duration::from_nanos(9100 * 10 + 9100 / 4), 1822);
        assert_eq!(e.lines, 10.25);
        assert_eq!(e.code, 164);
        assert_eq!(t.exposure_code_to_lines(164), 10.25);
        t.exposure.fraction_bits = 0;
        t.exposure.step = 4;
        t.exposure.min = 2;
        assert_eq!(
            t.exposure(Duration::from_nanos(9100 * 11), 1822).lines,
            10.0
        );
        // Max 1810: steps from 2 are 1806 and 1810.
        assert_eq!(t.exposure(Duration::from_secs(1), 1822).lines, 1810.0);
        assert_eq!(t.exposure(Duration::from_secs(1), 1821).lines, 1806.0);
    }

    #[test]
    fn hblank_changes_line_length() {
        let t = timing().with_hblank(250);
        assert_eq!(t.line_length(), 1530);
        assert_eq!(timing().with_hblank(1).hblank, 176);
    }
}
