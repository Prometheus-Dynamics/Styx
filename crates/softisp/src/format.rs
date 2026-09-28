//! Raw input formats: colour filter pattern, sample packing, frame geometry.

use serde::{Deserialize, Serialize};
use styx_core::prelude::FourCc;

use crate::simd::{raw10_bytes, raw12_bytes};

/// The colour filter array's top-left 2x2 quad, row by row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CfaPattern {
    Rggb,
    Bggr,
    Grbg,
    Gbrg,
}

/// A colour channel of the mosaic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    Red,
    Green,
    Blue,
}

impl CfaPattern {
    /// The colour at column `x`, row `y`.
    pub fn channel_at(self, x: usize, y: usize) -> Channel {
        let quad = match self {
            Self::Rggb => [Channel::Red, Channel::Green, Channel::Green, Channel::Blue],
            Self::Bggr => [Channel::Blue, Channel::Green, Channel::Green, Channel::Red],
            Self::Grbg => [Channel::Green, Channel::Red, Channel::Blue, Channel::Green],
            Self::Gbrg => [Channel::Green, Channel::Blue, Channel::Red, Channel::Green],
        };
        quad[(y & 1) * 2 + (x & 1)]
    }

    /// Index into a `[red, green-on-red-row, green-on-blue-row, blue]` array (the order of
    /// per-channel black levels) for column `x`, row `y`.
    pub fn cell_at(self, x: usize, y: usize) -> usize {
        match self.channel_at(x, y) {
            Channel::Red => 0,
            Channel::Blue => 3,
            Channel::Green => {
                // A green shares its row with red or with blue.
                if self.channel_at(x ^ 1, y) == Channel::Red {
                    1
                } else {
                    2
                }
            }
        }
    }
}

/// How samples are stored in a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawPacking {
    /// One byte per sample.
    U8,
    /// Little-endian 16-bit words holding `bits`-bit samples (`bits` 9 to 16).
    U16Le { bits: u8 },
    /// MIPI CSI-2 packed RAW10: four samples in five bytes (V4L2 `pBAA` and relatives).
    Csi2Raw10,
    /// MIPI CSI-2 packed RAW12: two samples in three bytes (V4L2 `pBCC` and relatives).
    Csi2Raw12,
}

impl RawPacking {
    /// Significant bits per sample.
    pub fn bit_depth(self) -> u8 {
        match self {
            Self::U8 => 8,
            Self::U16Le { bits } => bits,
            Self::Csi2Raw10 => 10,
            Self::Csi2Raw12 => 12,
        }
    }

    /// Bytes holding a row of `width` samples.
    pub fn row_bytes(self, width: usize) -> usize {
        match self {
            Self::U8 => width,
            Self::U16Le { .. } => width * 2,
            Self::Csi2Raw10 => raw10_bytes(width),
            Self::Csi2Raw12 => raw12_bytes(width),
        }
    }
}

/// A raw Bayer frame's layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RawFormat {
    pub width: u32,
    pub height: u32,
    pub pattern: CfaPattern,
    pub packing: RawPacking,
}

impl RawFormat {
    pub fn new(width: u32, height: u32, pattern: CfaPattern, packing: RawPacking) -> Self {
        Self {
            width,
            height,
            pattern,
            packing,
        }
    }

    /// The layout of a V4L2 Bayer pixel format of the given size, `None` for other formats.
    pub fn from_fourcc(code: FourCc, width: u32, height: u32) -> Option<Self> {
        let (pattern, packing) = bayer_fourcc(code)?;
        Some(Self::new(width, height, pattern, packing))
    }

    /// The smallest row stride.
    pub fn min_stride(&self) -> usize {
        self.packing.row_bytes(self.width as usize)
    }
}

/// Pattern and packing of the V4L2 Bayer formats.
pub fn bayer_fourcc(code: FourCc) -> Option<(CfaPattern, RawPacking)> {
    use CfaPattern::*;
    use RawPacking::*;
    let le = |bits| U16Le { bits };
    Some(match &code.to_u32().to_le_bytes() {
        b"BA81" | b"BGGR" => (Bggr, U8),
        b"GBRG" => (Gbrg, U8),
        b"GRBG" => (Grbg, U8),
        b"RGGB" => (Rggb, U8),
        b"BG10" => (Bggr, le(10)),
        b"GB10" => (Gbrg, le(10)),
        b"BA10" | b"GR10" => (Grbg, le(10)),
        b"RG10" => (Rggb, le(10)),
        b"BG12" => (Bggr, le(12)),
        b"GB12" => (Gbrg, le(12)),
        b"BA12" | b"GR12" => (Grbg, le(12)),
        b"RG12" => (Rggb, le(12)),
        b"BG14" => (Bggr, le(14)),
        b"GB14" => (Gbrg, le(14)),
        b"BA14" | b"GR14" => (Grbg, le(14)),
        b"RG14" => (Rggb, le(14)),
        b"BG16" | b"BYR2" => (Bggr, le(16)),
        b"GB16" => (Gbrg, le(16)),
        b"GR16" => (Grbg, le(16)),
        b"RG16" => (Rggb, le(16)),
        b"pBAA" => (Bggr, Csi2Raw10),
        b"pGAA" => (Gbrg, Csi2Raw10),
        b"pgAA" => (Grbg, Csi2Raw10),
        b"pRAA" => (Rggb, Csi2Raw10),
        b"pBCC" => (Bggr, Csi2Raw12),
        b"pGCC" => (Gbrg, Csi2Raw12),
        b"pgCC" => (Grbg, Csi2Raw12),
        b"pRCC" => (Rggb, Csi2Raw12),
        _ => return None,
    })
}

/// Every V4L2 Bayer format [`bayer_fourcc`] knows.
pub const BAYER_FOURCCS: [[u8; 4]; 33] = [
    *b"BA81", *b"BGGR", *b"GBRG", *b"GRBG", *b"RGGB", *b"BG10", *b"GB10", *b"BA10", *b"GR10",
    *b"RG10", *b"BG12", *b"GB12", *b"BA12", *b"GR12", *b"RG12", *b"BG14", *b"GB14", *b"BA14",
    *b"GR14", *b"RG14", *b"BG16", *b"BYR2", *b"GB16", *b"GR16", *b"RG16", *b"pBAA", *b"pGAA",
    *b"pgAA", *b"pRAA", *b"pBCC", *b"pGCC", *b"pgCC", *b"pRCC",
];
