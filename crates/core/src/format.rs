use std::{fmt, num::NonZeroU32, str::FromStr};

/// Four-character code describing a pixel/stream format.
///
/// # Example
/// ```rust
/// use styx_core::prelude::FourCc;
///
/// let fcc = FourCc::MJPG;
/// assert_eq!(fcc.to_string(), "MJPG");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = String, example = "MJPG"))]
pub struct FourCc([u8; 4]);

/// Shared metadata for a known FourCC format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatInfo {
    pub fourcc: FourCc,
    pub compressed: bool,
    pub jpeg_encoded: bool,
    pub packed_bytes_per_pixel: Option<usize>,
    pub default_color_space: Option<ColorSpace>,
}

impl FormatInfo {
    /// Estimate the byte size for a common single-frame representation.
    pub fn estimated_frame_bytes(self, width: usize, height: usize) -> Option<usize> {
        let pixels = width.checked_mul(height)?;
        match self.fourcc {
            FourCc::R16 | FourCc::YUYV | FourCc::UYVY | FourCc::YVYU | FourCc::VYUY => {
                pixels.checked_mul(2)
            }
            FourCc::NV12 | FourCc::NV21 | FourCc::I420 | FourCc::YU12 | FourCc::YV12 => {
                pixels.checked_mul(3).map(|bytes| bytes.div_ceil(2))
            }
            FourCc::XR24 | FourCc::XB24 | FourCc::D32F => pixels.checked_mul(4),
            FourCc::RG48 | FourCc::BG48 => pixels.checked_mul(6),
            _ => self
                .packed_bytes_per_pixel
                .and_then(|bytes_per_pixel| pixels.checked_mul(bytes_per_pixel)),
        }
    }
}

impl FourCc {
    pub const R8: Self = Self::new(*b"R8  ");
    pub const GREY: Self = Self::new(*b"GREY");
    pub const RG24: Self = Self::new(*b"RG24");
    pub const RGB3: Self = Self::new(*b"RGB3");
    pub const BGR3: Self = Self::new(*b"BGR3");
    pub const BG24: Self = Self::new(*b"BG24");
    pub const RGBA: Self = Self::new(*b"RGBA");
    pub const BGRA: Self = Self::new(*b"BGRA");
    pub const NV12: Self = Self::new(*b"NV12");
    pub const NV21: Self = Self::new(*b"NV21");
    pub const YUYV: Self = Self::new(*b"YUYV");
    pub const UYVY: Self = Self::new(*b"UYVY");
    pub const YVYU: Self = Self::new(*b"YVYU");
    pub const VYUY: Self = Self::new(*b"VYUY");
    pub const I420: Self = Self::new(*b"I420");
    pub const YU12: Self = Self::new(*b"YU12");
    pub const YV12: Self = Self::new(*b"YV12");
    pub const D32F: Self = Self::new(*b"D32F");
    pub const R16: Self = Self::new(*b"R16 ");
    pub const XR24: Self = Self::new(*b"XR24");
    pub const XB24: Self = Self::new(*b"XB24");
    pub const RG48: Self = Self::new(*b"RG48");
    pub const BG48: Self = Self::new(*b"BG48");
    pub const MJPG: Self = Self::new(*b"MJPG");
    pub const JPEG: Self = Self::new(*b"JPEG");
    pub const H264: Self = Self::new(*b"H264");
    pub const H265: Self = Self::new(*b"H265");
    pub const HEVC: Self = Self::new(*b"HEVC");

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

    /// Whether this format is stored as a compressed packet rather than raw image planes.
    pub fn is_compressed(self) -> bool {
        self.info().compressed
    }

    /// Whether this FourCC names a JPEG-encoded stream or packet.
    pub fn is_jpeg_encoded(self) -> bool {
        self.info().jpeg_encoded
    }

    /// Bytes per pixel for common single-plane packed raw formats.
    pub fn packed_bytes_per_pixel(self) -> Option<usize> {
        self.info().packed_bytes_per_pixel
    }

    /// Estimated byte size for common raw frame formats.
    pub fn estimated_frame_bytes(self, width: usize, height: usize) -> Option<usize> {
        self.info().estimated_frame_bytes(width, height)
    }

    /// Shared metadata for this FourCC.
    pub fn info(self) -> FormatInfo {
        let compressed = matches!(
            self,
            Self::MJPG | Self::JPEG | Self::H264 | Self::H265 | Self::HEVC
        );
        let jpeg_encoded = matches!(self, Self::MJPG | Self::JPEG);
        let packed_bytes_per_pixel = match self {
            Self::R8 | Self::GREY => Some(1),
            Self::RG24 | Self::RGB3 | Self::BGR3 | Self::BG24 => Some(3),
            Self::RGBA | Self::BGRA => Some(4),
            _ => None,
        };
        let bytes = self.to_u32().to_le_bytes();
        let default_color_space = match self {
            Self::MJPG
            | Self::JPEG
            | Self::RG24
            | Self::RGB3
            | Self::BGR3
            | Self::BG24
            | Self::RGBA
            | Self::BGRA
            | Self::XB24
            | Self::XR24 => Some(ColorSpace::Srgb),
            Self::NV12
            | Self::NV21
            | Self::YUYV
            | Self::YVYU
            | Self::UYVY
            | Self::VYUY
            | Self::I420
            | Self::YU12
            | Self::YV12 => Some(ColorSpace::Bt709),
            _ => match &bytes {
                b"RGB6" => Some(ColorSpace::Srgb),
                b"NV16" | b"NV61" | b"NV24" | b"NV42" | b"YU16" | b"YV16" | b"YU24" | b"YV24" => {
                    Some(ColorSpace::Bt709)
                }
                _ => None,
            },
        };
        FormatInfo {
            fourcc: self,
            compressed,
            jpeg_encoded,
            packed_bytes_per_pixel,
            default_color_space,
        }
    }
}

impl From<u32> for FourCc {
    fn from(value: u32) -> Self {
        Self(value.to_le_bytes())
    }
}

impl From<[u8; 4]> for FourCc {
    fn from(value: [u8; 4]) -> Self {
        Self(value)
    }
}

impl From<&[u8; 4]> for FourCc {
    fn from(value: &[u8; 4]) -> Self {
        Self(*value)
    }
}

impl TryFrom<&str> for FourCc {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<String> for FourCc {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.as_str().try_into()
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

#[cfg(test)]
mod fourcc_tests {
    use super::FourCc;

    #[test]
    fn classifies_compressed_formats() {
        assert!(FourCc::MJPG.is_compressed());
        assert!(FourCc::H264.is_compressed());
        assert!(!FourCc::RG24.is_compressed());
        assert!(FourCc::JPEG.is_jpeg_encoded());
        assert!(!FourCc::H264.is_jpeg_encoded());
    }

    #[test]
    fn estimates_common_raw_frame_bytes() {
        assert_eq!(FourCc::RG24.estimated_frame_bytes(640, 480), Some(921_600));
        assert_eq!(FourCc::NV12.estimated_frame_bytes(4, 2), Some(12));
        assert_eq!(FourCc::MJPG.estimated_frame_bytes(4, 2), None);
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
/// use styx_core::prelude::Interval;
///
/// let interval = Interval::from_fps(30).unwrap();
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
    /// Create a frame interval from whole frames-per-second.
    ///
    /// Returns `None` when `fps` is zero.
    pub fn from_fps(fps: u32) -> Option<Self> {
        Some(Self {
            numerator: NonZeroU32::new(1)?,
            denominator: NonZeroU32::new(fps)?,
        })
    }

    /// Create a frame interval from a rational seconds-per-frame value.
    ///
    /// Returns `None` when either side of the rational is zero.
    pub fn new(numerator: u32, denominator: u32) -> Option<Self> {
        Some(Self {
            numerator: NonZeroU32::new(numerator)?,
            denominator: NonZeroU32::new(denominator)?,
        })
    }

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
/// use styx_core::prelude::{Interval, IntervalStepwise};
///
/// let stepwise = IntervalStepwise {
///     min: Interval::from_fps(60).unwrap(),
///     max: Interval::from_fps(30).unwrap(),
///     step: Interval::from_fps(30).unwrap(),
/// };
/// assert!(stepwise.contains(Interval::from_fps(30).unwrap()));
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
/// use styx_core::prelude::{FourCc, MediaFormat};
///
/// let fmt = MediaFormat::srgb(FourCc::RG24, 1920, 1080).unwrap();
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

    /// Build an sRGB format from dimensions.
    ///
    /// Returns `None` when either dimension is zero.
    pub fn srgb(code: FourCc, width: u32, height: u32) -> Option<Self> {
        Some(Self::new(
            code,
            Resolution::new(width, height)?,
            ColorSpace::Srgb,
        ))
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
