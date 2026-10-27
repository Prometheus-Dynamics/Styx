//! A frame's colour space hint and its wire code.

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

impl ColorSpace {
    /// The byte recordings, the camera service's wire and previews carry for it (stable: 0
    /// sRGB, 1 BT.709, 2 BT.2020, 3 unknown).
    pub const fn code(self) -> u8 {
        match self {
            Self::Srgb => 0,
            Self::Bt709 => 1,
            Self::Bt2020 => 2,
            Self::Unknown => 3,
        }
    }

    /// The colour space of [`ColorSpace::code`] `code`; codes it does not know are `Unknown`.
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Srgb,
            1 => Self::Bt709,
            2 => Self::Bt2020,
            _ => Self::Unknown,
        }
    }
}
