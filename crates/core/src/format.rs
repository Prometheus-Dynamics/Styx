use std::{fmt, num::NonZeroU32, str::FromStr};

/// Four-character code describing a pixel/stream format.
///
/// # Example
/// ```rust
/// use styx_core::prelude::FourCc;
///
/// let fcc = FourCc::new(*b"MJPG");
/// assert_eq!(fcc.to_string(), "MJPG");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = String, example = "MJPG"))]
pub struct FourCc([u8; 4]);

impl FourCc {
    /// Construct from raw bytes.
    pub const fn new(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    /// Little-endian u32 encoding.
    pub fn to_u32(self) -> u32 {
        u32::from_le_bytes(self.0)
    }

    /// Try to convert to a printable string.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl From<u32> for FourCc {
    fn from(value: u32) -> Self {
        Self(value.to_le_bytes())
    }
}

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.as_str() {
            write!(f, "{s}")
        } else {
            write!(f, "0x{:08x}", self.to_u32())
        }
    }
}

impl FromStr for FourCc {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 4 {
            return Err("fourcc must be four ASCII bytes".into());
        }
        let mut arr = [0u8; 4];
        arr.copy_from_slice(bytes);
        Ok(FourCc(arr))
    }
}

/// Resolution of a frame.
///
/// # Example
/// ```rust
/// use styx_core::prelude::Resolution;
///
/// let res = Resolution::new(640, 480).unwrap();
/// assert_eq!(res.width.get(), 640);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct Resolution {
    /// Width in pixels (non-zero).
    #[cfg_attr(feature = "schema", schema(value_type = u32, minimum = 1))]
    pub width: NonZeroU32,
    /// Height in pixels (non-zero).
    #[cfg_attr(feature = "schema", schema(value_type = u32, minimum = 1))]
    pub height: NonZeroU32,
}

impl Resolution {
    /// Create a resolution, returning `None` if width or height are zero.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Some(Self {
            width: NonZeroU32::new(width)?,
            height: NonZeroU32::new(height)?,
        })
    }
}

/// Frame interval (fps) expressed as a rational.
///
/// # Example
/// ```rust
/// use std::num::NonZeroU32;
/// use styx_core::prelude::Interval;
///
/// let interval = Interval {
///     numerator: NonZeroU32::new(1).unwrap(),
///     denominator: NonZeroU32::new(30).unwrap(),
/// };
/// assert!(interval.fps() > 0.0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct Interval {
    /// Numerator of the fps rational.
    #[cfg_attr(feature = "schema", schema(value_type = u32, minimum = 1))]
    pub numerator: NonZeroU32,
    /// Denominator of the fps rational.
    #[cfg_attr(feature = "schema", schema(value_type = u32, minimum = 1))]
    pub denominator: NonZeroU32,
}

impl Interval {
    /// Frames per second as floating point.
    pub fn fps(&self) -> f32 {
        // V4L2 expresses frame intervals as a fraction of seconds per frame
        // (numerator / denominator), so fps is the inverse.
        self.denominator.get() as f32 / self.numerator.get() as f32
    }

    pub fn within(&self, min: Interval, max: Interval) -> bool {
        // Compare as rational: self between min and max.
        let self_num = self.numerator.get() as u64;
        let self_den = self.denominator.get() as u64;
        let min_num = min.numerator.get() as u64;
        let min_den = min.denominator.get() as u64;
        let max_num = max.numerator.get() as u64;
        let max_den = max.denominator.get() as u64;
        self_num * min_den >= min_num * self_den && self_num * max_den <= max_num * self_den
    }
}

/// Stepwise interval description (min/max/step).
///
/// # Example
/// ```rust
/// use std::num::NonZeroU32;
/// use styx_core::prelude::{Interval, IntervalStepwise};
///
/// let make = |n, d| Interval {
///     numerator: NonZeroU32::new(n).unwrap(),
///     denominator: NonZeroU32::new(d).unwrap(),
/// };
/// let stepwise = IntervalStepwise {
///     min: make(1, 60),
///     max: make(1, 30),
///     step: make(1, 30),
/// };
/// assert!(stepwise.contains(make(1, 30)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct IntervalStepwise {
    pub min: Interval,
    pub max: Interval,
    pub step: Interval,
}

impl IntervalStepwise {
    pub fn contains(&self, candidate: Interval) -> bool {
        if !candidate.within(self.min, self.max) {
            return false;
        }
        // Rough step check: compare fps spacing.
        let step_fps = self.step.fps();
        if step_fps == 0.0 {
            return true;
        }
        let min_fps = self.min.fps();
        let cand_fps = candidate.fps();
        let steps = ((cand_fps - min_fps) / step_fps).round();
        ((cand_fps - min_fps) - steps * step_fps).abs() < 0.001
    }
}

/// Basic color space hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum ColorSpace {
    /// Standard sRGB.
    Srgb,
    /// Rec. 709.
    Bt709,
    /// Rec. 2020.
    Bt2020,
    /// Unspecified/unknown.
    Unknown,
}

/// Media format including code and geometry.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{ColorSpace, FourCc, MediaFormat, Resolution};
///
/// let res = Resolution::new(1920, 1080).unwrap();
/// let fmt = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
/// assert_eq!(fmt.code.to_string(), "RG24");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct MediaFormat {
    /// FourCc code describing pixel layout.
    #[cfg_attr(feature = "schema", schema(value_type = String))]
    pub code: FourCc,
    /// Resolution of the frame.
    pub resolution: Resolution,
    /// Color space hint.
    pub color: ColorSpace,
}

impl MediaFormat {
    /// Build a new format.
    pub fn new(code: FourCc, resolution: Resolution, color: ColorSpace) -> Self {
        Self {
            code,
            resolution,
            color,
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for FourCc {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Prefer string encoding so decoding does not rely on `deserialize_any`.
        let encoded = self.as_str().unwrap_or("FFFF");
        serializer.serialize_str(encoded)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for FourCc {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct FourCcVisitor;

        impl<'de> serde::de::Visitor<'de> for FourCcVisitor {
            type Value = FourCc;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a 4-character FourCc string")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                FourCc::from_str(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(FourCcVisitor)
    }
}
