//! Colour spaces and ranges.

/// The colour space of a frame's samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ColorSpace {
    /// sRGB.
    Srgb,
    /// Rec. 709.
    Bt709,
    /// Rec. 2020.
    Bt2020,
    /// Rec. 601 (JPEG's YCbCr).
    Bt601,
    /// Not known.
    Unknown,
}

impl ColorSpace {
    /// A stable one-byte code for recordings and wire formats: 0 sRGB, 1 BT.709, 2 BT.2020,
    /// 3 unknown, 4 BT.601.
    pub const fn code(self) -> u8 {
        match self {
            Self::Srgb => 0,
            Self::Bt709 => 1,
            Self::Bt2020 => 2,
            Self::Unknown => 3,
            Self::Bt601 => 4,
        }
    }

    /// The colour space of [`ColorSpace::code`]; codes it does not know are `Unknown`.
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Srgb,
            1 => Self::Bt709,
            2 => Self::Bt2020,
            4 => Self::Bt601,
            _ => Self::Unknown,
        }
    }
}

/// Which part of the sample range carries the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ColorRange {
    /// Every code value (0 to 255 for 8 bits).
    Full,
    /// Video range (16 to 235 luma, 16 to 240 chroma for 8 bits).
    Limited,
    /// Not known.
    Unknown,
}
