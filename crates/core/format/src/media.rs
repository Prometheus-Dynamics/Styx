//! Frame size, frame interval and the two with a format.

use core::num::NonZeroU32;

use crate::{ColorRange, ColorSpace, FourCc};

/// A frame's width and height in pixels, both non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Resolution {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl Resolution {
    /// `None` when either side is zero.
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        match (NonZeroU32::new(width), NonZeroU32::new(height)) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            _ => None,
        }
    }
}

/// Seconds per frame, as V4L2 gives it: `numerator / denominator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Interval {
    pub numerator: NonZeroU32,
    pub denominator: NonZeroU32,
}

impl Interval {
    /// `numerator / denominator` seconds per frame, `None` when either is zero.
    pub const fn new(numerator: u32, denominator: u32) -> Option<Self> {
        match (NonZeroU32::new(numerator), NonZeroU32::new(denominator)) {
            (Some(numerator), Some(denominator)) => Some(Self {
                numerator,
                denominator,
            }),
            _ => None,
        }
    }

    /// One frame per `1 / fps` seconds, `None` for zero.
    pub const fn from_fps(fps: u32) -> Option<Self> {
        Self::new(1, fps)
    }

    /// Frames per second.
    pub fn fps(&self) -> f32 {
        self.denominator.get() as f32 / self.numerator.get() as f32
    }

    /// Frames per second as `(numerator, denominator)`.
    const fn rate(self) -> (u64, u64) {
        (self.denominator.get() as u64, self.numerator.get() as u64)
    }

    /// Whether this interval lies between `shortest` and `longest`, inclusive.
    pub const fn within(&self, shortest: Interval, longest: Interval) -> bool {
        let (n, d) = (self.numerator.get() as u64, self.denominator.get() as u64);
        let (sn, sd) = (
            shortest.numerator.get() as u64,
            shortest.denominator.get() as u64,
        );
        let (ln, ld) = (
            longest.numerator.get() as u64,
            longest.denominator.get() as u64,
        );
        n * sd >= sn * d && n * ld <= ln * d
    }
}

/// A range of intervals: from `min` to `max` in rate steps of `step`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct IntervalStepwise {
    /// The shortest interval (the highest rate).
    pub min: Interval,
    /// The longest interval.
    pub max: Interval,
    /// The rate step, as an interval (`1/30` steps the rate by 30 fps).
    pub step: Interval,
}

impl IntervalStepwise {
    /// Whether `candidate` is in the range and a whole number of rate steps from its highest
    /// rate (exact, in integers).
    pub const fn contains(&self, candidate: Interval) -> bool {
        if !candidate.within(self.min, self.max) {
            return false;
        }
        // Rates as fractions: candidate cn/cd, highest hn/hd, step sn/sd. The candidate is
        // (hn/hd - cn/cd) / (sn/sd) = (hn*cd - cn*hd) * sd / (hd*cd*sn) steps below.
        let (cn, cd) = candidate.rate();
        let (hn, hd) = self.min.rate();
        let (sn, sd) = self.step.rate();
        let above = (hn as u128 * cd as u128).saturating_sub(cn as u128 * hd as u128);
        let num = above * sd as u128;
        let den = hd as u128 * cd as u128 * sn as u128;
        num.is_multiple_of(den)
    }
}

/// A frame format: code, size, colour space and range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MediaFormat {
    pub code: FourCc,
    pub resolution: Resolution,
    pub color: ColorSpace,
    pub range: ColorRange,
}

impl MediaFormat {
    pub const fn new(
        code: FourCc,
        resolution: Resolution,
        color: ColorSpace,
        range: ColorRange,
    ) -> Self {
        Self {
            code,
            resolution,
            color,
            range,
        }
    }

    /// `code`'s default colour space and range; unknown where the format has none.
    pub fn with_defaults(code: FourCc, resolution: Resolution) -> Self {
        let info = code.info();
        Self::new(
            code,
            resolution,
            info.and_then(|i| i.default_color)
                .unwrap_or(ColorSpace::Unknown),
            info.map_or(ColorRange::Unknown, |i| i.default_range),
        )
    }
}
