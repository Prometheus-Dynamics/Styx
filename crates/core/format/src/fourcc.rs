//! The format code.

use core::fmt;
use core::str::FromStr;

/// A four-character format code, V4L2-style: the four bytes in memory order, as the kernel's
/// `v4l2_fourcc` packs them (little-endian [`FourCc::to_u32`]).
///
/// ```
/// use styx_format::FourCc;
///
/// assert_eq!(FourCc::NV12.to_string(), "NV12");
/// assert_eq!("NV12".parse::<FourCc>(), Ok(FourCc::NV12));
/// assert_eq!(FourCc::from(FourCc::NV12.to_u32()), FourCc::NV12);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FourCc([u8; 4]);

/// A string that is not four bytes long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseFourCcError;

impl fmt::Display for ParseFourCcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a FourCC is four bytes")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseFourCcError {}

impl FourCc {
    // Greyscale and single channel.
    pub const R8: Self = Self::new(*b"R8  ");
    pub const GREY: Self = Self::new(*b"GREY");
    pub const Y10: Self = Self::new(*b"Y10 ");
    pub const Y12: Self = Self::new(*b"Y12 ");
    pub const Y14: Self = Self::new(*b"Y14 ");
    pub const Y16: Self = Self::new(*b"Y16 ");
    pub const R16: Self = Self::new(*b"R16 ");
    pub const Y10P: Self = Self::new(*b"Y10P");
    pub const Y12P: Self = Self::new(*b"Y12P");
    pub const D32F: Self = Self::new(*b"D32F");
    // Packed RGB, bytes in memory order.
    pub const RG24: Self = Self::new(*b"RG24");
    pub const RGB3: Self = Self::new(*b"RGB3");
    pub const BG24: Self = Self::new(*b"BG24");
    pub const BGR3: Self = Self::new(*b"BGR3");
    pub const RGBA: Self = Self::new(*b"RGBA");
    pub const BGRA: Self = Self::new(*b"BGRA");
    pub const XR24: Self = Self::new(*b"XR24");
    pub const XB24: Self = Self::new(*b"XB24");
    pub const RG48: Self = Self::new(*b"RG48");
    pub const RGB6: Self = Self::new(*b"RGB6");
    pub const BG48: Self = Self::new(*b"BG48");
    // Packed YUV 4:2:2.
    pub const YUYV: Self = Self::new(*b"YUYV");
    pub const UYVY: Self = Self::new(*b"UYVY");
    pub const YVYU: Self = Self::new(*b"YVYU");
    pub const VYUY: Self = Self::new(*b"VYUY");
    // Semi-planar YUV.
    pub const NV12: Self = Self::new(*b"NV12");
    pub const NV21: Self = Self::new(*b"NV21");
    pub const NV16: Self = Self::new(*b"NV16");
    pub const NV61: Self = Self::new(*b"NV61");
    pub const NV24: Self = Self::new(*b"NV24");
    pub const NV42: Self = Self::new(*b"NV42");
    // Planar YUV.
    pub const I420: Self = Self::new(*b"I420");
    pub const YU12: Self = Self::new(*b"YU12");
    pub const YV12: Self = Self::new(*b"YV12");
    pub const YU16: Self = Self::new(*b"YU16");
    pub const YV16: Self = Self::new(*b"YV16");
    pub const YU24: Self = Self::new(*b"YU24");
    pub const YV24: Self = Self::new(*b"YV24");
    // Bayer, one sample per byte.
    pub const RGGB: Self = Self::new(*b"RGGB");
    pub const GRBG: Self = Self::new(*b"GRBG");
    pub const GBRG: Self = Self::new(*b"GBRG");
    pub const BGGR: Self = Self::new(*b"BGGR");
    pub const BA81: Self = Self::new(*b"BA81");
    // Bayer, one sample per little-endian 16-bit word.
    pub const RG10: Self = Self::new(*b"RG10");
    pub const BA10: Self = Self::new(*b"BA10");
    pub const GR10: Self = Self::new(*b"GR10");
    pub const GB10: Self = Self::new(*b"GB10");
    pub const BG10: Self = Self::new(*b"BG10");
    pub const RG12: Self = Self::new(*b"RG12");
    pub const BA12: Self = Self::new(*b"BA12");
    pub const GR12: Self = Self::new(*b"GR12");
    pub const GB12: Self = Self::new(*b"GB12");
    pub const BG12: Self = Self::new(*b"BG12");
    pub const RG14: Self = Self::new(*b"RG14");
    pub const GR14: Self = Self::new(*b"GR14");
    pub const BA14: Self = Self::new(*b"BA14");
    pub const GB14: Self = Self::new(*b"GB14");
    pub const BG14: Self = Self::new(*b"BG14");
    pub const RG16: Self = Self::new(*b"RG16");
    pub const GR16: Self = Self::new(*b"GR16");
    pub const GB16: Self = Self::new(*b"GB16");
    pub const BG16: Self = Self::new(*b"BG16");
    pub const BYR2: Self = Self::new(*b"BYR2");
    // Bayer, MIPI CSI-2 packed (RAW10: 4 samples in 5 bytes, RAW12: 2 in 3).
    pub const RGGB10P: Self = Self::new(*b"pRAA");
    pub const GRBG10P: Self = Self::new(*b"pgAA");
    pub const GBRG10P: Self = Self::new(*b"pGAA");
    pub const BGGR10P: Self = Self::new(*b"pBAA");
    pub const RGGB12P: Self = Self::new(*b"pRCC");
    pub const GRBG12P: Self = Self::new(*b"pgCC");
    pub const GBRG12P: Self = Self::new(*b"pGCC");
    pub const BGGR12P: Self = Self::new(*b"pBCC");
    // Compressed.
    pub const MJPG: Self = Self::new(*b"MJPG");
    pub const JPEG: Self = Self::new(*b"JPEG");
    pub const H264: Self = Self::new(*b"H264");
    pub const H265: Self = Self::new(*b"H265");
    pub const HEVC: Self = Self::new(*b"HEVC");

    /// The code of these four bytes.
    pub const fn new(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    /// The four bytes.
    pub const fn bytes(self) -> [u8; 4] {
        self.0
    }

    /// The code as V4L2 and DRM number it: the bytes little-endian.
    pub const fn to_u32(self) -> u32 {
        u32::from_le_bytes(self.0)
    }

    /// The code as text, when its bytes are UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.0).ok()
    }

    /// The facts of this format, `None` for a code Styx does not know.
    pub fn info(self) -> Option<&'static FormatInfo> {
        crate::info::lookup(self)
    }

    /// A compressed packet (JPEG, H.264, H.265), not image planes.
    pub fn is_compressed(self) -> bool {
        self.info()
            .is_some_and(|i| i.storage == crate::Storage::Compressed)
    }

    /// A JPEG stream or packet.
    pub fn is_jpeg(self) -> bool {
        self.info().is_some_and(|i| i.jpeg)
    }

    /// A raw Bayer mosaic (any depth or packing).
    pub fn is_bayer(self) -> bool {
        self.info().is_some_and(|i| i.cfa.is_some())
    }

    /// An 8-bit luma plane comes first: grey, or 8-bit planar or semi-planar YUV.
    pub fn has_luma_plane(self) -> bool {
        self.info().is_some_and(|i| {
            i.sample_bits == 8
                && (matches!(
                    i.storage,
                    crate::Storage::Planar | crate::Storage::SemiPlanar
                ) || i
                    .packed
                    .is_some_and(|p| p.order == crate::PackedChannelOrder::Luma))
        })
    }
}

use crate::FormatInfo;

impl From<u32> for FourCc {
    fn from(value: u32) -> Self {
        Self(value.to_le_bytes())
    }
}

impl From<FourCc> for u32 {
    fn from(value: FourCc) -> Self {
        value.to_u32()
    }
}

impl From<[u8; 4]> for FourCc {
    fn from(value: [u8; 4]) -> Self {
        Self(value)
    }
}

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_str() {
            Some(s) => f.write_str(s),
            None => write!(f, "0x{:08x}", self.to_u32()),
        }
    }
}

impl FromStr for FourCc {
    type Err = ParseFourCcError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes: [u8; 4] = s.as_bytes().try_into().map_err(|_| ParseFourCcError)?;
        Ok(Self(bytes))
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for FourCc {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.as_str() {
            Some(s) => serializer.serialize_str(s),
            None => serializer.serialize_u32(self.to_u32()),
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for FourCc {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = FourCc;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a four-character FourCC or its number")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<FourCc, E> {
                v.parse().map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<FourCc, E> {
                u32::try_from(v).map(FourCc::from).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}
