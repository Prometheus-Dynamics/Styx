//! Back end block defaults.
//!
//! Values from libpisp `src/libpisp/backend/backend_default_config.json` and the conversions
//! in `backend_default_config.cpp` (BSD-2-Clause, Copyright (C) 2021 - 2023, Raspberry Pi
//! Ltd), ported to Rust as constants.

use crate::uapi::*;

/// Default debin coefficients.
pub const DEBIN_COEFFS: [i8; 4] = [-7, 105, 35, -5];

/// Default demosaic set-up.
pub const DEMOSAIC: BeDemosaicConfig = BeDemosaicConfig {
    sharper: 8,
    fc_mode: 1,
    pad: [0; 2],
};

/// Default false colour distance.
pub const FALSE_COLOUR: BeFalseColourConfig = BeFalseColourConfig {
    distance: 2,
    pad: [0; 3],
};

/// libpisp's default gamma curve, `(x, y)` points on 16-bit scales.
pub const GAMMA_POINTS: [(u32, u32); 33] = [
    (0, 0),
    (1024, 5040),
    (2048, 9338),
    (3072, 12356),
    (4096, 15312),
    (5120, 18051),
    (6144, 20790),
    (7168, 23193),
    (8192, 25744),
    (9216, 27942),
    (10240, 30035),
    (11264, 32005),
    (12288, 33975),
    (13312, 35815),
    (14336, 37600),
    (15360, 39168),
    (16384, 40642),
    (18432, 43379),
    (20480, 45749),
    (22528, 47753),
    (24576, 49621),
    (26624, 51253),
    (28672, 52698),
    (30720, 53796),
    (32768, 54876),
    (36864, 57012),
    (40960, 58656),
    (45056, 59954),
    (49152, 61183),
    (53248, 62355),
    (57344, 63419),
    (61440, 64476),
    (65535, 65535),
];

/// Evaluates a piecewise-linear curve given as sorted `(x, y)` points (truncating, as
/// libpisp's `Pwl::Eval` result is assigned to an int).
pub fn pwl_eval(points: &[(u32, u32)], x: u32) -> u32 {
    let last = points.len() - 2;
    let mut span = 0;
    while span < last && x >= points[span + 1].0 {
        span += 1;
    }
    let (x0, y0) = (f64::from(points[span].0), f64::from(points[span].1));
    let (x1, y1) = (f64::from(points[span + 1].0), f64::from(points[span + 1].1));
    if x1 == x0 {
        return y1 as u32;
    }
    (y0 + (f64::from(x) - x0) * (y1 - y0) / (x1 - x0)) as u32
}

/// Builds the gamma block from a curve (`(x, y)` points, 16-bit scales): 64 entries at the
/// hardware's knots, each `slope << 16 | y`.
pub fn gamma_from_curve(points: &[(u32, u32)]) -> BeGammaConfig {
    const SLOPE_BITS: u32 = 14;
    let mut lut = [0u32; BE_GAMMA_LUT_SIZE];
    let mut last_y = 0u32;
    for i in 0..BE_GAMMA_LUT_SIZE as u32 {
        let x = if i < 32 {
            i * 512
        } else if i < 48 {
            (i - 32) * 1024 + 16384
        } else {
            ((i - 48) * 2048 + 32768).min(65535)
        };
        let mut y = pwl_eval(points, x).max(last_y);
        if i > 0 {
            let mut slope = y - last_y;
            if slope >= 1 << SLOPE_BITS {
                slope = (1 << SLOPE_BITS) - 1;
                y = last_y + slope;
            }
            lut[i as usize - 1] |= slope << 16;
        }
        lut[i as usize] = y;
        last_y = y;
    }
    BeGammaConfig { lut }
}

/// A named YCbCr encoding: forward (RGB to YCbCr) and inverse matrices.
#[derive(Clone, Copy, Debug)]
pub struct Encoding {
    /// Name as libpisp spells it.
    pub name: &'static str,
    /// RGB to YCbCr.
    pub ycbcr: BeCcmConfig,
    /// YCbCr to RGB.
    pub inverse: BeCcmConfig,
}

const fn ccm(coeffs: [i16; 9], offsets: [i32; 3]) -> BeCcmConfig {
    BeCcmConfig {
        coeffs,
        pad: [0; 2],
        offsets,
    }
}

/// libpisp's colour encodings.
pub const ENCODINGS: [Encoding; 6] = [
    Encoding {
        name: "jpeg",
        ycbcr: ccm(
            [306, 601, 117, -173, -339, 512, 512, -429, -83],
            [0, 33554432, 33554432],
        ),
        inverse: ccm(
            [1024, 0, 1436, 1024, -352, -731, 1024, 1815, 0],
            [-47043259, 35509710, -59458469],
        ),
    },
    Encoding {
        name: "smpte170m",
        ycbcr: ccm(
            [263, 516, 100, -152, -298, 450, 450, -377, -73],
            [4194304, 33554432, 33554432],
        ),
        inverse: ccm(
            [1192, 0, 1634, 1192, -401, -832, 1192, 2066, 0],
            [-58437489, 35540222, -72570875],
        ),
    },
    Encoding {
        name: "rec709",
        ycbcr: ccm(
            [187, 629, 63, -103, -347, 450, 450, -409, -41],
            [4194304, 33554432, 33554432],
        ),
        inverse: ccm(
            [1192, 0, 1836, 1192, -218, -546, 1192, 2163, 0],
            [-65031074, 20151458, -75768630],
        ),
    },
    Encoding {
        name: "rec709_full",
        ycbcr: ccm(
            [218, 732, 74, -117, -395, 512, 512, -465, -47],
            [0, 33554432, 33554432],
        ),
        inverse: ccm(
            [1024, 0, 1613, 1024, -192, -479, 1024, 1900, 0],
            [-52835271, 21991737, -62267477],
        ),
    },
    Encoding {
        name: "bt2020",
        ycbcr: ccm(
            [231, 596, 52, -126, -324, 450, 450, -414, -36],
            [4194304, 33554432, 33554432],
        ),
        inverse: ccm(
            [1192, 0, 1719, 1192, -192, -666, 1192, 2193, 0],
            [-61210735, 23226461, -76749732],
        ),
    },
    Encoding {
        name: "bt2020_full",
        ycbcr: ccm(
            [269, 694, 61, -143, -369, 512, 512, -471, -41],
            [0, 33554432, 33554432],
        ),
        inverse: ccm(
            [1024, 0, 1510, 1024, -168, -585, 1024, 1927, 0],
            [-49479366, 24692917, -63129308],
        ),
    },
];

/// Looks up an encoding by name.
pub fn encoding(name: &str) -> Option<&'static Encoding> {
    ENCODINGS.iter().find(|e| e.name == name)
}

/// Resampling filter: Lanczos 3.
pub const LANCZOS3: [i16; 96] = [
    -15, 16, 979, 71, -31, 4, -2, -30, 965, 132, -48, 7, 9, -69, 939, 200, -65, 10, 18, -99, 901,
    273, -83, 14, 24, -121, 854, 350, -101, 18, 28, -135, 796, 429, -116, 22, 30, -143, 731, 509,
    -129, 26, 30, -143, 660, 587, -139, 29, 29, -139, 587, 660, -143, 30, 26, -129, 509, 731, -143,
    30, 22, -116, 429, 796, -135, 28, 18, -101, 350, 854, -121, 24, 14, -83, 273, 901, -99, 18, 10,
    -65, 200, 939, -69, 9, 7, -48, 132, 965, -30, -2, 4, -31, 71, 979, 16, -15,
];

/// Resampling filter: Lanczos 2.
pub const LANCZOS2: [i16; 96] = [
    -55, 231, 638, 266, -54, -2, -55, 197, 637, 301, -51, -5, -53, 165, 628, 337, -46, -7, -51,
    134, 617, 373, -39, -10, -47, 105, 602, 408, -30, -14, -43, 79, 582, 442, -18, -18, -38, 54,
    561, 474, -4, -23, -33, 32, 535, 505, 13, -28, -28, 13, 505, 535, 32, -33, -23, -4, 474, 561,
    54, -38, -18, -18, 442, 582, 79, -43, -14, -30, 408, 602, 105, -47, -10, -39, 373, 617, 134,
    -51, -7, -46, 337, 628, 165, -53, -5, -51, 301, 637, 197, -55, -2, -54, 266, 638, 231, -55,
];

/// Resampling filter: Mitchell-Netravali.
pub const MICHEL_NETRAVALI: [i16; 96] = [
    -24, 217, 604, 249, -22, 0, -25, 186, 600, 282, -19, 0, -25, 156, 594, 315, -15, -1, -24, 129,
    584, 348, -10, -3, -23, 103, 571, 381, -3, -5, -21, 80, 554, 413, 6, -8, -18, 59, 533, 444, 16,
    -10, -16, 42, 510, 473, 28, -13, -13, 28, 473, 510, 42, -16, -10, 16, 444, 533, 59, -18, -8, 6,
    413, 554, 80, -21, -5, -3, 381, 571, 103, -23, -3, -10, 348, 584, 129, -24, -1, -15, 315, 594,
    156, -25, 0, -19, 282, 600, 186, -25, 0, -22, 249, 604, 217, -24,
];

/// The filter libpisp's "smart selection" picks for a downscale factor: Lanczos 3 up to
/// 0.5, Lanczos 2 up to 2.0, Mitchell-Netravali beyond.
pub fn resample_filter_for(downscale: f64) -> [i16; 96] {
    if downscale <= 0.5 {
        LANCZOS3
    } else if downscale <= 2.0 {
        LANCZOS2
    } else {
        MICHEL_NETRAVALI
    }
}

/// Default sharpening and the sharpen/false-colour combine factors.
pub fn sharpen() -> (BeSharpenConfig, BeShFcCombineConfig) {
    let s = BeSharpenConfig {
        kernel0: [
            -2, -2, -2, -2, -2, -1, -1, -1, -1, -1, 6, 6, 6, 6, 6, -1, -1, -1, -1, -1, -2, -2, -2,
            -2, -2,
        ],
        kernel1: [
            -2, -1, 6, -1, -2, -2, -1, 6, -1, -2, -2, -1, 6, -1, -2, -2, -1, 6, -1, -2, -2, -1, 6,
            -1, -2,
        ],
        kernel2: [
            2, -1, -2, 0, 0, -1, 5, -1, -2, 0, -2, -1, 6, -1, -2, 0, -2, -1, 5, -1, 0, 0, -2, -1, 2,
        ],
        kernel3: [
            0, 0, -2, -1, 2, 0, -2, -1, 5, -1, -2, -1, 6, -1, -2, -1, 5, -1, -2, 0, 2, -1, -2, 0, 0,
        ],
        kernel4: [
            -1, -2, -2, -2, -1, -2, 2, 2, 2, -2, -2, 2, 12, 2, -2, -2, 2, 2, 2, -2, -1, -2, -2, -2,
            -1,
        ],
        thresholds: [
            [100, 614, 512, 0],
            [100, 614, 512, 0],
            [100, 614, 921, 0],
            [100, 614, 921, 0],
            [200, 552, 512, 0],
        ],
        positive_strength: 76,
        positive_pre_limit: 2000,
        positive_func: [512, 1024, 1536, 2048, 3072, 4096, 5120, 6656, 8192],
        positive_limit: 4000,
        negative_strength: 102,
        negative_pre_limit: 2000,
        negative_func: [512, 716, 1536, 2048, 3072, 4096, 5120, 6656, 8192],
        negative_limit: 4000,
        enables: 0x1f,
        white: 0,
        black: 0,
        grey: 50,
        ..Default::default()
    };
    let shfc = BeShFcCombineConfig {
        y_factor: (0.75 * 256.0) as u8,
        ..Default::default()
    };
    (s, shfc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamma_is_monotonic_with_slopes() {
        let g = gamma_from_curve(&GAMMA_POINTS);
        assert_eq!(g.lut[0] & 0xffff, 0);
        // The last knot is x = 63488, on the (61440, 64476)..(65535, 65535) span.
        assert_eq!(g.lut[63] & 0xffff, 65005);
        for i in 1..BE_GAMMA_LUT_SIZE {
            let (y0, y1) = (g.lut[i - 1] & 0xffff, g.lut[i] & 0xffff);
            assert!(y1 >= y0);
            assert_eq!(g.lut[i - 1] >> 16, y1 - y0);
        }
        // x = 512 lies between (0, 0) and (1024, 5040).
        assert_eq!(g.lut[1] & 0xffff, 2520);
    }

    #[test]
    fn filters_and_encodings() {
        for f in [LANCZOS3, LANCZOS2, MICHEL_NETRAVALI] {
            for phase in f.chunks(6) {
                let sum: i32 = phase.iter().map(|&c| i32::from(c)).sum();
                assert!((sum - 1024).abs() <= 2, "{sum}");
            }
        }
        assert_eq!(encoding("jpeg").unwrap().ycbcr.coeffs[0], 306);
        assert!(encoding("nope").is_none());
        assert_eq!(resample_filter_for(1.0), LANCZOS2);
    }
}
