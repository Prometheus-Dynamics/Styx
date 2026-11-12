//! The facts of each format, as one table.

use crate::{ColorRange, ColorSpace, FourCc};

/// How a format's bytes are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Storage {
    /// One plane of whole pixels (RGB, packed YUV, grey).
    Packed,
    /// Luma, then one plane of interleaved chroma.
    SemiPlanar,
    /// Luma, then one plane per chroma channel.
    Planar,
    /// One plane of a colour filter mosaic.
    RawBayer,
    /// A compressed packet, no image planes.
    Compressed,
}

/// A channel of a packed pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Channel {
    Red,
    Green,
    Blue,
    Alpha,
    /// An unused byte (the `X` of `XR24` / `XB24`).
    Padding,
    Luma,
    ChromaBlue,
    ChromaRed,
    Depth,
}

/// The order of a packed pixel's channels, in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PackedChannelOrder {
    Luma,
    Rgb,
    Bgr,
    Rgba,
    Bgra,
    /// Bytes R, G, B, x (`XB24`).
    Rgbx,
    /// Bytes B, G, R, x (`XR24`).
    Bgrx,
    Yuyv,
    Uyvy,
    Yvyu,
    Vyuy,
    Depth,
    /// One mosaic sample (see [`FormatInfo::cfa`] for which).
    RawBayer,
}

impl PackedChannelOrder {
    /// The channels of one pixel (two for the 4:2:2 orders, which share chroma), in memory
    /// order.
    pub const fn channels(self) -> &'static [Channel] {
        use Channel::*;
        match self {
            Self::Luma => &[Luma],
            Self::Rgb => &[Red, Green, Blue],
            Self::Bgr => &[Blue, Green, Red],
            Self::Rgba => &[Red, Green, Blue, Alpha],
            Self::Bgra => &[Blue, Green, Red, Alpha],
            Self::Rgbx => &[Red, Green, Blue, Padding],
            Self::Bgrx => &[Blue, Green, Red, Padding],
            Self::Yuyv => &[Luma, ChromaBlue, Luma, ChromaRed],
            Self::Uyvy => &[ChromaBlue, Luma, ChromaRed, Luma],
            Self::Yvyu => &[Luma, ChromaRed, Luma, ChromaBlue],
            Self::Vyuy => &[ChromaRed, Luma, ChromaBlue, Luma],
            Self::Depth => &[Depth],
            Self::RawBayer => &[Red, Green, Blue],
        }
    }
}

/// The container of one sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BitDepth {
    U8,
    U16,
    F32,
    U8x3,
    U8x4,
    U16x3,
    /// Samples packed across bytes (MIPI CSI-2 RAW10 / RAW12).
    Packed,
    /// A compressed packet.
    Compressed,
}

/// Chroma subsampling of a YUV format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ChromaSubsampling {
    Cs420,
    Cs422,
    Cs444,
}

/// The colour filter pattern of a Bayer format: the top-left 2x2 block, row by row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CfaOrder {
    Rggb,
    Grbg,
    Gbrg,
    Bggr,
}

/// A packed pixel: its bytes and channel order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PackedPixel {
    pub bytes_per_pixel: u8,
    pub order: PackedChannelOrder,
}

/// What Styx knows about a format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FormatInfo {
    pub fourcc: FourCc,
    pub storage: Storage,
    /// Whole-byte pixels of one plane (none for planar YUV, CSI-2 packed and compressed).
    pub packed: Option<PackedPixel>,
    /// Image planes (0 for compressed).
    pub planes: u8,
    pub subsampling: Option<ChromaSubsampling>,
    pub bit_depth: BitDepth,
    /// Significant bits per sample (0 for compressed).
    pub sample_bits: u8,
    /// Bits per sample in a CSI-2 packed row (rows are `width * bits / 8` bytes, rounded up).
    pub csi2_bits: Option<u8>,
    /// The mosaic of a Bayer format.
    pub cfa: Option<CfaOrder>,
    /// A JPEG stream or packet.
    pub jpeg: bool,
    /// The colour space a frame has unless told otherwise.
    pub default_color: Option<ColorSpace>,
    /// The range a frame has unless told otherwise.
    pub default_range: ColorRange,
}

const fn base(code: &[u8; 4], storage: Storage, depth: BitDepth, bits: u8) -> FormatInfo {
    FormatInfo {
        fourcc: FourCc::new(*code),
        storage,
        packed: None,
        planes: 1,
        subsampling: None,
        bit_depth: depth,
        sample_bits: bits,
        csi2_bits: None,
        cfa: None,
        jpeg: false,
        default_color: None,
        default_range: ColorRange::Full,
    }
}

const fn packed(
    code: &[u8; 4],
    bytes: u8,
    order: PackedChannelOrder,
    depth: BitDepth,
    bits: u8,
    color: Option<ColorSpace>,
) -> FormatInfo {
    let mut f = base(code, Storage::Packed, depth, bits);
    f.packed = Some(PackedPixel {
        bytes_per_pixel: bytes,
        order,
    });
    f.default_color = color;
    f
}

const fn rgb(code: &[u8; 4], bytes: u8, order: PackedChannelOrder, depth: BitDepth) -> FormatInfo {
    let bits = if matches!(depth, BitDepth::U16x3) {
        16
    } else {
        8
    };
    let color = if bits == 8 {
        Some(ColorSpace::Srgb)
    } else {
        None
    };
    packed(code, bytes, order, depth, bits, color)
}

const fn grey(code: &[u8; 4], bytes: u8, bits: u8) -> FormatInfo {
    let depth = if bytes == 1 {
        BitDepth::U8
    } else {
        BitDepth::U16
    };
    packed(code, bytes, PackedChannelOrder::Luma, depth, bits, None)
}

const fn yuv422(code: &[u8; 4], order: PackedChannelOrder) -> FormatInfo {
    let mut f = packed(code, 2, order, BitDepth::U8, 8, Some(ColorSpace::Bt709));
    f.default_range = ColorRange::Limited;
    f
}

const fn yuv(code: &[u8; 4], storage: Storage, planes: u8, sub: ChromaSubsampling) -> FormatInfo {
    let mut f = base(code, storage, BitDepth::U8, 8);
    f.planes = planes;
    f.subsampling = Some(sub);
    f.default_color = Some(ColorSpace::Bt709);
    f.default_range = ColorRange::Limited;
    f
}

const fn bayer(code: &[u8; 4], bytes: u8, bits: u8, cfa: CfaOrder) -> FormatInfo {
    let depth = if bytes == 1 {
        BitDepth::U8
    } else {
        BitDepth::U16
    };
    let mut f = base(code, Storage::RawBayer, depth, bits);
    f.packed = Some(PackedPixel {
        bytes_per_pixel: bytes,
        order: PackedChannelOrder::RawBayer,
    });
    f.cfa = Some(cfa);
    f
}

const fn csi2(code: &[u8; 4], bits: u8, cfa: Option<CfaOrder>) -> FormatInfo {
    let storage = if cfa.is_some() {
        Storage::RawBayer
    } else {
        Storage::Packed
    };
    let mut f = base(code, storage, BitDepth::Packed, bits);
    f.csi2_bits = Some(bits);
    f.cfa = cfa;
    f
}

const fn compressed(code: &[u8; 4], jpeg: bool) -> FormatInfo {
    let mut f = base(code, Storage::Compressed, BitDepth::Compressed, 0);
    f.planes = 0;
    f.jpeg = jpeg;
    if jpeg {
        f.default_color = Some(ColorSpace::Srgb);
    } else {
        f.default_range = ColorRange::Unknown;
    }
    f
}

/// Every format Styx knows. Two codes for the same bytes (`RG24` and `RGB3`, `I420` and
/// `YU12`, ...) each have a row.
pub const FORMATS: &[FormatInfo] = {
    use BitDepth::*;
    use CfaOrder::*;
    use ChromaSubsampling::*;
    use PackedChannelOrder as O;
    use Storage::*;
    &[
        // Greyscale and single channel.
        grey(b"R8  ", 1, 8),
        grey(b"GREY", 1, 8),
        grey(b"Y10 ", 2, 10),
        grey(b"Y12 ", 2, 12),
        grey(b"Y14 ", 2, 14),
        grey(b"Y16 ", 2, 16),
        grey(b"R16 ", 2, 16),
        csi2(b"Y10P", 10, None),
        csi2(b"Y12P", 12, None),
        packed(b"D32F", 4, O::Depth, F32, 32, None),
        // Packed RGB, bytes in memory order.
        rgb(b"RG24", 3, O::Rgb, U8x3),
        rgb(b"RGB3", 3, O::Rgb, U8x3),
        rgb(b"BG24", 3, O::Bgr, U8x3),
        rgb(b"BGR3", 3, O::Bgr, U8x3),
        rgb(b"RGBA", 4, O::Rgba, U8x4),
        rgb(b"BGRA", 4, O::Bgra, U8x4),
        rgb(b"XR24", 4, O::Bgrx, U8x4),
        rgb(b"XB24", 4, O::Rgbx, U8x4),
        rgb(b"RG48", 6, O::Rgb, U16x3),
        rgb(b"RGB6", 6, O::Rgb, U16x3),
        rgb(b"BG48", 6, O::Bgr, U16x3),
        // Packed YUV 4:2:2.
        yuv422(b"YUYV", O::Yuyv),
        yuv422(b"UYVY", O::Uyvy),
        yuv422(b"YVYU", O::Yvyu),
        yuv422(b"VYUY", O::Vyuy),
        // Semi-planar YUV.
        yuv(b"NV12", SemiPlanar, 2, Cs420),
        yuv(b"NV21", SemiPlanar, 2, Cs420),
        yuv(b"NV16", SemiPlanar, 2, Cs422),
        yuv(b"NV61", SemiPlanar, 2, Cs422),
        yuv(b"NV24", SemiPlanar, 2, Cs444),
        yuv(b"NV42", SemiPlanar, 2, Cs444),
        // Planar YUV.
        yuv(b"I420", Planar, 3, Cs420),
        yuv(b"YU12", Planar, 3, Cs420),
        yuv(b"YV12", Planar, 3, Cs420),
        yuv(b"YU16", Planar, 3, Cs422),
        yuv(b"YV16", Planar, 3, Cs422),
        yuv(b"YU24", Planar, 3, Cs444),
        yuv(b"YV24", Planar, 3, Cs444),
        // Bayer, one sample per byte (`BA81` is V4L2's 8-bit BGGR).
        bayer(b"RGGB", 1, 8, Rggb),
        bayer(b"GRBG", 1, 8, Grbg),
        bayer(b"GBRG", 1, 8, Gbrg),
        bayer(b"BGGR", 1, 8, Bggr),
        bayer(b"BA81", 1, 8, Bggr),
        // Bayer, one sample per little-endian 16-bit word (V4L2's codes; `BA14` is libcamera's
        // 14-bit GRBG, `BYR2` V4L2's 16-bit BGGR).
        bayer(b"RG10", 2, 10, Rggb),
        bayer(b"BA10", 2, 10, Grbg),
        bayer(b"GR10", 2, 10, Grbg),
        bayer(b"GB10", 2, 10, Gbrg),
        bayer(b"BG10", 2, 10, Bggr),
        bayer(b"RG12", 2, 12, Rggb),
        bayer(b"BA12", 2, 12, Grbg),
        bayer(b"GR12", 2, 12, Grbg),
        bayer(b"GB12", 2, 12, Gbrg),
        bayer(b"BG12", 2, 12, Bggr),
        bayer(b"RG14", 2, 14, Rggb),
        bayer(b"GR14", 2, 14, Grbg),
        bayer(b"BA14", 2, 14, Grbg),
        bayer(b"GB14", 2, 14, Gbrg),
        bayer(b"BG14", 2, 14, Bggr),
        bayer(b"RG16", 2, 16, Rggb),
        bayer(b"GR16", 2, 16, Grbg),
        bayer(b"GB16", 2, 16, Gbrg),
        bayer(b"BG16", 2, 16, Bggr),
        bayer(b"BYR2", 2, 16, Bggr),
        // Bayer, MIPI CSI-2 packed.
        csi2(b"pRAA", 10, Some(Rggb)),
        csi2(b"pgAA", 10, Some(Grbg)),
        csi2(b"pGAA", 10, Some(Gbrg)),
        csi2(b"pBAA", 10, Some(Bggr)),
        csi2(b"pRCC", 12, Some(Rggb)),
        csi2(b"pgCC", 12, Some(Grbg)),
        csi2(b"pGCC", 12, Some(Gbrg)),
        csi2(b"pBCC", 12, Some(Bggr)),
        // Compressed.
        compressed(b"MJPG", true),
        compressed(b"JPEG", true),
        compressed(b"H264", false),
        compressed(b"H265", false),
        compressed(b"HEVC", false),
    ]
};

/// The row of `code`.
pub(crate) fn lookup(code: FourCc) -> Option<&'static FormatInfo> {
    FORMATS.iter().find(|f| f.fourcc == code)
}

impl FormatInfo {
    /// Bytes of the visible part of the first plane's row, `width` pixels wide (`None` for
    /// compressed formats).
    pub const fn first_plane_row_bytes(&self, width: usize) -> Option<usize> {
        if let Some(p) = self.packed {
            return width.checked_mul(p.bytes_per_pixel as usize);
        }
        if let Some(bits) = self.csi2_bits {
            return match width.checked_mul(bits as usize) {
                Some(b) => Some(b.div_ceil(8)),
                None => None,
            };
        }
        match self.storage {
            Storage::Planar | Storage::SemiPlanar => Some(width),
            _ => None,
        }
    }
}
