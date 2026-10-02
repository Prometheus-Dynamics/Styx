//! Back end lens shading tables: gains per grid vertex, packed the way the hardware reads them.
//!
//! The packing (`pack_lut`) and the cell-centre to corner resampling (`resample_table`) follow
//! the Raspberry Pi PiSP IPA, libcamera `src/ipa/rpi/pisp/pisp.cpp` (`packLscLut`,
//! `resampleTable`; BSD-2-Clause, Copyright (C) 2023 Raspberry Pi Ltd), rewritten in Rust.

use crate::uapi::{BE_LSC_LUT_SIZE, BeLscConfig};

/// Gains per vertex of the 33x33 grid, `[channel (R, G, B)][row][column]`.
pub type LscTable = [[[f64; BE_LSC_LUT_SIZE]; BE_LSC_LUT_SIZE]; 3];

fn field(v: f64, bits: u32) -> u32 {
    v.clamp(0.0, f64::from((1u32 << bits) - 1)) as u32
}

/// Packs the gains: each vertex codes R, G and B in 10 bits each, in one of four ranges
/// (`[0.5, 1.5)`, `[0, 2)`, `[0, 4)`, `[0, 8)`) chosen by the vertex's smallest and largest
/// gain. Grid steps are left at zero (spread over the input image when prepared).
pub fn pack_lut(rgb: &LscTable) -> BeLscConfig {
    let mut cfg = BeLscConfig {
        grid_step_x: 0,
        grid_step_y: 0,
        lut_packed: [[0; BE_LSC_LUT_SIZE]; BE_LSC_LUT_SIZE],
    };
    for y in 0..BE_LSC_LUT_SIZE {
        for x in 0..BE_LSC_LUT_SIZE {
            let v = [rgb[0][y][x], rgb[1][y][x], rgb[2][y][x]];
            let lo = v.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let (range, scale, offset) = if lo >= 0.5 && hi < 1.5 {
                (0u32, 1024.0, -511.5)
            } else if hi < 2.0 {
                (1, 512.0, 0.5)
            } else if hi < 4.0 {
                (2, 256.0, 0.5)
            } else {
                (3, 128.0, 0.5)
            };
            let [r, g, b] = v.map(|c| field(offset + scale * c, 10));
            cfg.lut_packed[y][x] = (range << 30) | (b << 20) | (g << 10) | r;
        }
    }
    cfg
}

/// Resamples a `src_w` x `src_h` table sampled at cell centres to the 33x33 vertices of the
/// back end's grid (sampled at the corners), bilinearly, clamping at the edges.
pub fn resample_table(
    src: &[f64],
    src_w: usize,
    src_h: usize,
) -> [[f64; BE_LSC_LUT_SIZE]; BE_LSC_LUT_SIZE] {
    let n = BE_LSC_LUT_SIZE;
    let mut out = [[1.0; BE_LSC_LUT_SIZE]; BE_LSC_LUT_SIZE];
    if src_w == 0 || src_h == 0 || src.len() < src_w * src_h {
        return out;
    }
    let sample = |count: usize, len: usize| -> Vec<(usize, usize, f64)> {
        let inc = len as f64 / (count - 1) as f64;
        (0..count)
            .map(|i| {
                let p = -0.5 + i as f64 * inc;
                let lo = p.floor();
                let f = p - lo;
                let lo = lo as isize;
                let hi = if lo < len as isize - 1 {
                    lo + 1
                } else {
                    len as isize - 1
                };
                (lo.max(0) as usize, hi.max(0) as usize, f)
            })
            .collect()
    };
    let xs = sample(n, src_w);
    let ys = sample(n, src_h);
    for (j, &(y0, y1, yf)) in ys.iter().enumerate() {
        for (i, &(x0, x1, xf)) in xs.iter().enumerate() {
            let at = |x: usize, y: usize| src[y * src_w + x];
            let above = at(x0, y0) * (1.0 - xf) + at(x1, y0) * xf;
            let below = at(x0, y1) * (1.0 - xf) + at(x1, y1) * xf;
            out[j][i] = above * (1.0 - yf) + below * yf;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_picks_the_range_and_codes_unity() {
        let mut t: LscTable = [[[1.0; BE_LSC_LUT_SIZE]; BE_LSC_LUT_SIZE]; 3];
        t[0][0][1] = 1.9;
        t[2][0][2] = 3.0;
        t[1][0][3] = 5.0;
        let c = pack_lut(&t);
        let unpack = |v: u32| (v >> 30, v & 0x3ff, (v >> 10) & 0x3ff, (v >> 20) & 0x3ff);
        // Range 0: 1.0 → 512.5 - 0.5 → 512.
        assert_eq!(unpack(c.lut_packed[0][0]), (0, 512, 512, 512));
        assert_eq!(unpack(c.lut_packed[0][1]).0, 1);
        assert_eq!(unpack(c.lut_packed[0][1]).1, (0.5 + 512.0 * 1.9) as u32);
        assert_eq!(unpack(c.lut_packed[0][2]).0, 2);
        assert_eq!(unpack(c.lut_packed[0][3]), (3, 128, 640, 128));
    }

    #[test]
    fn resampling_keeps_flat_tables_and_follows_ramps() {
        let flat = vec![1.25; 32 * 32];
        let r = resample_table(&flat, 32, 32);
        assert!(r.iter().flatten().all(|&v| (v - 1.25).abs() < 1e-12));
        // A horizontal ramp over 16 cell centres (0..15): corners at -0.5..15.5, clamped.
        let ramp: Vec<f64> = (0..16 * 12).map(|i| (i % 16) as f64).collect();
        let r = resample_table(&ramp, 16, 12);
        assert_eq!(r[5][0], 0.0);
        assert_eq!(r[5][32], 15.0);
        assert!((r[5][16] - 7.5).abs() < 1e-9);
    }
}
