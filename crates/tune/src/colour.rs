//! Colour science for the chart: the ColorChecker's reference colours, sRGB ↔ XYZ ↔ CIELAB, and
//! colour differences (ΔE 1976 and CIEDE2000).
//!
//! The reference colours are the widely published sRGB (D65) values of the 24-patch
//! ColorChecker Classic, 8-bit, row by row from dark skin to black; the colour matrices map to
//! linear sRGB, as the Raspberry Pi IPA's do.

use crate::linalg::{Mat3, mul_vec};

/// The ColorChecker Classic in sRGB (D65, 8 bit), patches 1-24 row by row.
pub const MACBETH_SRGB: [[u8; 3]; 24] = [
    [116, 81, 67],
    [199, 147, 129],
    [91, 122, 156],
    [90, 108, 64],
    [130, 128, 176],
    [92, 190, 172],
    [224, 124, 47],
    [68, 91, 170],
    [198, 82, 97],
    [94, 58, 106],
    [159, 189, 63],
    [230, 162, 39],
    [35, 63, 147],
    [67, 149, 74],
    [180, 49, 57],
    [238, 198, 20],
    [193, 84, 151],
    [0, 136, 170],
    [245, 245, 243],
    [200, 202, 202],
    [161, 163, 163],
    [121, 121, 122],
    [82, 84, 86],
    [49, 49, 51],
];

/// Names of the patches.
pub const MACBETH_NAMES: [&str; 24] = [
    "dark skin",
    "light skin",
    "blue sky",
    "foliage",
    "blue flower",
    "bluish green",
    "orange",
    "purplish blue",
    "moderate red",
    "purple",
    "yellow green",
    "orange yellow",
    "blue",
    "green",
    "red",
    "yellow",
    "magenta",
    "cyan",
    "white 9.5",
    "neutral 8",
    "neutral 6.5",
    "neutral 5",
    "neutral 3.5",
    "black 2",
];

/// The neutral patches (white to black).
pub const GREYS: std::ops::Range<usize> = 18..24;

/// The greys used for white balance: the middle four (white may clip, black is noisy).
pub const WB_GREYS: std::ops::Range<usize> = 19..23;

/// sRGB transfer function inverse: encoded `[0, 1]` → linear.
pub fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB transfer function: linear → encoded.
pub fn linear_to_srgb(v: f64) -> f64 {
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.max(0.0).powf(1.0 / 2.4) - 0.055
    }
}

/// The reference patches in linear sRGB, `[0, 1]`.
pub fn macbeth_linear() -> [[f64; 3]; 24] {
    MACBETH_SRGB.map(|p| p.map(|c| srgb_to_linear(f64::from(c) / 255.0)))
}

/// Linear sRGB → XYZ (D65).
pub const SRGB_TO_XYZ: Mat3 = [
    0.412_456_4,
    0.357_576_1,
    0.180_437_5,
    0.212_672_9,
    0.715_152_2,
    0.072_175_0,
    0.019_333_9,
    0.119_192,
    0.950_304_1,
];

/// The D65 white point (Y = 1).
pub const D65: [f64; 3] = [0.950_47, 1.0, 1.088_83];

/// XYZ → CIELAB relative to `white`.
pub fn xyz_to_lab(xyz: [f64; 3], white: [f64; 3]) -> [f64; 3] {
    let f = |t: f64| {
        const D: f64 = 6.0 / 29.0;
        if t > D * D * D {
            t.cbrt()
        } else {
            t / (3.0 * D * D) + 4.0 / 29.0
        }
    };
    let [fx, fy, fz] = [0, 1, 2].map(|i| f(xyz[i] / white[i]));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// Linear sRGB → CIELAB (D65).
pub fn linear_srgb_to_lab(rgb: [f64; 3]) -> [f64; 3] {
    xyz_to_lab(mul_vec(&SRGB_TO_XYZ, rgb), D65)
}

/// ΔE 1976: Euclidean distance in CIELAB.
pub fn delta_e76(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// CIEDE2000 (kL = kC = kH = 1).
pub fn delta_e2000(lab1: [f64; 3], lab2: [f64; 3]) -> f64 {
    use std::f64::consts::PI;
    let deg = |r: f64| r * 180.0 / PI;
    let rad = |d: f64| d * PI / 180.0;
    let [l1, a1, b1] = lab1;
    let [l2, a2, b2] = lab2;
    let c_bar = ((a1 * a1 + b1 * b1).sqrt() + (a2 * a2 + b2 * b2).sqrt()) / 2.0;
    let g = 0.5 * (1.0 - (c_bar.powi(7) / (c_bar.powi(7) + 25f64.powi(7))).sqrt());
    let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
    let (c1p, c2p) = ((a1p * a1p + b1 * b1).sqrt(), (a2p * a2p + b2 * b2).sqrt());
    let hue = |b: f64, a: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            let h = deg(b.atan2(a));
            if h < 0.0 { h + 360.0 } else { h }
        }
    };
    let (h1p, h2p) = (hue(b1, a1p), hue(b2, a2p));
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let dh = if c1p * c2p == 0.0 {
        0.0
    } else if (h2p - h1p).abs() <= 180.0 {
        h2p - h1p
    } else if h2p - h1p > 180.0 {
        h2p - h1p - 360.0
    } else {
        h2p - h1p + 360.0
    };
    let dhh = 2.0 * (c1p * c2p).sqrt() * rad(dh / 2.0).sin();
    let l_bar = (l1 + l2) / 2.0;
    let cp_bar = (c1p + c2p) / 2.0;
    let hp_bar = if c1p * c2p == 0.0 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        (h1p + h2p) / 2.0
    } else if h1p + h2p < 360.0 {
        (h1p + h2p + 360.0) / 2.0
    } else {
        (h1p + h2p - 360.0) / 2.0
    };
    let t = 1.0 - 0.17 * rad(hp_bar - 30.0).cos()
        + 0.24 * rad(2.0 * hp_bar).cos()
        + 0.32 * rad(3.0 * hp_bar + 6.0).cos()
        - 0.20 * rad(4.0 * hp_bar - 63.0).cos();
    let d_theta = 30.0 * (-((hp_bar - 275.0) / 25.0).powi(2)).exp();
    let rc = 2.0 * (cp_bar.powi(7) / (cp_bar.powi(7) + 25f64.powi(7))).sqrt();
    let sl = 1.0 + 0.015 * (l_bar - 50.0).powi(2) / (20.0 + (l_bar - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * cp_bar;
    let sh = 1.0 + 0.015 * cp_bar * t;
    let rt = -rad(2.0 * d_theta).sin() * rc;
    ((dl / sl).powi(2) + (dc / sc).powi(2) + (dhh / sh).powi(2) + rt * (dc / sc) * (dhh / sh))
        .sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_is_l100_and_srgb_round_trips() {
        let lab = linear_srgb_to_lab([1.0, 1.0, 1.0]);
        assert!(
            (lab[0] - 100.0).abs() < 0.01 && lab[1].abs() < 0.05 && lab[2].abs() < 0.05,
            "{lab:?}"
        );
        for v in [0.0, 0.002, 0.2, 0.9] {
            assert!((srgb_to_linear(linear_to_srgb(v)) - v).abs() < 1e-12);
        }
    }

    #[test]
    fn ciede2000_matches_published_pairs() {
        // Sharma, Wu and Dalal's test data, pairs 1, 7 and 17.
        let cases = [
            ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
            ([50.0, 0.0, 0.0], [50.0, -1.0, 2.0], 2.3669),
            ([50.0, 2.5, 0.0], [73.0, 25.0, -18.0], 27.1492),
        ];
        for (a, b, want) in cases {
            assert!((delta_e2000(a, b) - want).abs() < 1e-4, "{a:?} {b:?}");
        }
    }
}
