//! Small geometry types shared by the V4L2 and subdevice APIs: fractions and rectangles.

use std::fmt;

/// A fraction, used for frame intervals (seconds per frame) and pixel aspect ratios.
/// Layout-compatible with `struct v4l2_fract`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Fraction {
    /// Numerator.
    pub numerator: u32,
    /// Denominator.
    pub denominator: u32,
}

impl Fraction {
    /// Creates a fraction.
    pub const fn new(numerator: u32, denominator: u32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// The value as a float (`NaN` when the denominator is zero).
    pub fn as_f64(self) -> f64 {
        if self.denominator == 0 {
            f64::NAN
        } else {
            f64::from(self.numerator) / f64::from(self.denominator)
        }
    }

    /// The reciprocal: a frame interval in seconds becomes a rate in frames per second.
    pub fn recip(self) -> Self {
        Self::new(self.denominator, self.numerator)
    }

    /// Frames per second when this fraction is a frame interval.
    pub fn fps(self) -> f64 {
        self.recip().as_f64()
    }
}

impl fmt::Display for Fraction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

/// A rectangle. Layout-compatible with `struct v4l2_rect`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl Rect {
    /// Creates a rectangle.
    pub const fn new(left: i32, top: i32, width: u32, height: u32) -> Self {
        Self {
            left,
            top,
            width,
            height,
        }
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "({},{})/{}x{}",
            self.left, self.top, self.width, self.height
        )
    }
}

const _: () = assert!(size_of::<Fraction>() == 8);
const _: () = assert!(size_of::<Rect>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_fps() {
        let interval = Fraction::new(1, 30);
        assert_eq!(interval.fps(), 30.0);
        assert_eq!(interval.to_string(), "1/30");
        assert!(Fraction::new(1, 0).as_f64().is_nan());
    }
}
