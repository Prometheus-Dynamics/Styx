//! The formats the scalers handle, and which conversions between them are exact.
//!
//! Only conversions that move samples without a colour matrix are here: channel order and alpha
//! within the RGB family, chroma order and chroma planar/interleaved within the 4:2:0 family, and
//! the single-channel grey formats. A YUV to RGB conversion needs a colour matrix and a range; it
//! is not in this crate yet, so no candidate is registered for it.

use styx_format::{FourCc, LayoutError, PlaneGeometry, Planes, plane_geometry};

/// The formats this crate scales and converts.
pub const FORMATS: [FourCc; 8] = [
    FourCc::GREY,
    FourCc::R8,
    FourCc::RG24,
    FourCc::BG24,
    FourCc::BGRA,
    FourCc::NV12,
    FourCc::NV21,
    FourCc::YU12,
];

/// What a format's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One plane of single-channel samples: GREY, R8.
    Gray,
    /// One plane of interleaved pixels in `order`.
    Rgb(Rgb),
    /// Luma, then interleaved chroma pairs: NV12 is U then V, NV21 is V then U.
    SemiPlanar { vu: bool },
    /// Luma, then U and V in their own planes: YU12 and I420.
    Planar,
}

/// The channel order of an RGB-family pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rgb {
    /// RG24: R, G, B bytes. Three channels.
    Rgb24,
    /// BG24: B, G, R bytes. Three channels.
    Bgr24,
    /// BGRA: B, G, R, A bytes. Four channels.
    Bgra32,
}

impl Rgb {
    /// Bytes per pixel.
    pub const fn bpp(self) -> usize {
        match self {
            Self::Rgb24 | Self::Bgr24 => 3,
            Self::Bgra32 => 4,
        }
    }

    /// The byte index of red, green and blue in a pixel.
    pub const fn rgb_index(self) -> [usize; 3] {
        match self {
            Self::Rgb24 => [0, 1, 2],
            Self::Bgr24 | Self::Bgra32 => [2, 1, 0],
        }
    }
}

/// What `code` is, if this crate handles it.
pub fn kind(code: FourCc) -> Option<Kind> {
    Some(match code {
        FourCc::GREY | FourCc::R8 => Kind::Gray,
        FourCc::RG24 => Kind::Rgb(Rgb::Rgb24),
        FourCc::BG24 => Kind::Rgb(Rgb::Bgr24),
        FourCc::BGRA => Kind::Rgb(Rgb::Bgra32),
        FourCc::NV12 => Kind::SemiPlanar { vu: false },
        FourCc::NV21 => Kind::SemiPlanar { vu: true },
        FourCc::YU12 | FourCc::I420 => Kind::Planar,
        _ => return None,
    })
}

/// Whether `kind` is 4:2:0: its sides must be even.
pub const fn is_420(kind: Kind) -> bool {
    matches!(kind, Kind::SemiPlanar { .. } | Kind::Planar)
}

/// Whether samples can go from `input` to `output` with no colour matrix. Same size and format is
/// a copy, which is in the set.
pub fn convertible(input: FourCc, output: FourCc) -> bool {
    match (kind(input), kind(output)) {
        (Some(Kind::Gray), Some(Kind::Gray)) => true,
        (Some(Kind::Rgb(_)), Some(Kind::Rgb(_))) => true,
        (Some(a), Some(b)) => is_420(a) && is_420(b),
        _ => false,
    }
}

/// The filter a resize uses: area averaging when the frame gets no larger in either direction,
/// bilinear otherwise. The hardware and the software path choose the same way.
pub fn filter_for(sw: usize, sh: usize, dw: usize, dh: usize) -> Filter {
    if dw <= sw && dh <= sh {
        Filter::Area
    } else {
        Filter::Bilinear
    }
}

/// How a resize chooses each output sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Each output sample is the rounded mean of the input block it covers (at least one sample).
    Area,
    /// Bilinear interpolation at pixel centres, in 8-bit fixed point.
    Bilinear,
}

/// The planes of a `width` x `height` image of `code`, packed: each plane's rows at its visible
/// width, one after another.
pub fn packed(
    code: FourCc,
    width: usize,
    height: usize,
) -> Result<Planes<PlaneGeometry>, LayoutError> {
    plane_geometry(code, width as u32, height as u32)
}

/// Bytes of a packed `width` x `height` image of `code`, all planes.
pub fn packed_len(code: FourCc, width: usize, height: usize) -> Result<usize, LayoutError> {
    Ok(packed(code, width, height)?
        .iter()
        .map(|p| p.row_bytes * p.rows)
        .sum())
}

/// Whether `width` and `height` suit `code`: 4:2:0 needs even sides.
pub fn fits(code: FourCc, width: usize, height: usize) -> bool {
    match kind(code) {
        Some(k) if is_420(k) => {
            width.is_multiple_of(2) && height.is_multiple_of(2) && width > 0 && height > 0
        }
        Some(_) => width > 0 && height > 0,
        None => false,
    }
}
