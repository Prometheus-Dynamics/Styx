use styx_core::prelude::FourCc;

use super::BayerPattern;

#[derive(Clone, Copy)]
pub struct BayerInfo {
    pub(super) pattern: BayerPattern,
    pub(super) bit_depth: u8,
    pub(super) bytes_per_sample: usize,
}

pub fn bayer_info(fourcc: FourCc) -> Option<BayerInfo> {
    let code = fourcc.to_u32().to_le_bytes();
    let info = match &code {
        b"BA81" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"BA10" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"BA12" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"BA14" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"BG10" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"BG12" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"BG14" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"BG16" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"GB10" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"GB12" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"GB14" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"GB16" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"RG10" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"RG12" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"RG14" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"RG16" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"GR10" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 10,
            bytes_per_sample: 2,
        },
        b"GR12" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 12,
            bytes_per_sample: 2,
        },
        b"GR14" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 14,
            bytes_per_sample: 2,
        },
        b"GR16" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"BYR2" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 16,
            bytes_per_sample: 2,
        },
        b"RGGB" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"GRBG" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"GBRG" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"BGGR" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 8,
            bytes_per_sample: 1,
        },
        b"pBAA" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 10,
            bytes_per_sample: 0,
        },
        b"pGAA" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 10,
            bytes_per_sample: 0,
        },
        b"pgAA" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 10,
            bytes_per_sample: 0,
        },
        b"pRAA" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 10,
            bytes_per_sample: 0,
        },
        b"pBCC" => BayerInfo {
            pattern: BayerPattern::BGGR,
            bit_depth: 12,
            bytes_per_sample: 0,
        },
        b"pGCC" => BayerInfo {
            pattern: BayerPattern::GBRG,
            bit_depth: 12,
            bytes_per_sample: 0,
        },
        b"pgCC" => BayerInfo {
            pattern: BayerPattern::GRBG,
            bit_depth: 12,
            bytes_per_sample: 0,
        },
        b"pRCC" => BayerInfo {
            pattern: BayerPattern::RGGB,
            bit_depth: 12,
            bytes_per_sample: 0,
        },
        _ => return None,
    };
    Some(info)
}
