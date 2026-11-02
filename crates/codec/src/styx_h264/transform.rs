//! The 4x4 integer transform, quantisation and their inverses as the decoder computes them
//! (8.5.12 with flat scaling lists), and the DC transforms of Intra16x16 luma and chroma.
//! Blocks are raster order; levels come out in raster order too.

use super::tables::{DEQUANT_V, QUANT_MF, pos_class};

/// Levels are clamped here so CAVLC can always code them in Baseline (level_prefix ≤ 15).
const LEVEL_MAX: i32 = 2047;

/// The forward core transform of a 4x4 residual.
pub(super) fn fdct4(r: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for y in 0..4 {
        let s = &r[y * 4..][..4];
        let (a, b) = (s[0] + s[3], s[1] + s[2]);
        let (c, d) = (s[1] - s[2], s[0] - s[3]);
        t[y * 4] = a + b;
        t[y * 4 + 1] = 2 * d + c;
        t[y * 4 + 2] = a - b;
        t[y * 4 + 3] = d - 2 * c;
    }
    let mut o = [0i32; 16];
    for x in 0..4 {
        let (a, b) = (t[x] + t[12 + x], t[4 + x] + t[8 + x]);
        let (c, d) = (t[4 + x] - t[8 + x], t[x] - t[12 + x]);
        o[x] = a + b;
        o[4 + x] = 2 * d + c;
        o[8 + x] = a - b;
        o[12 + x] = d - 2 * c;
    }
    o
}

/// The inverse core transform of dequantised coefficients, rounded: the residual.
pub(super) fn idct4(d: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for y in 0..4 {
        let s = &d[y * 4..][..4];
        let (e0, e1) = (s[0] + s[2], s[0] - s[2]);
        let (e2, e3) = ((s[1] >> 1) - s[3], s[1] + (s[3] >> 1));
        t[y * 4] = e0 + e3;
        t[y * 4 + 1] = e1 + e2;
        t[y * 4 + 2] = e1 - e2;
        t[y * 4 + 3] = e0 - e3;
    }
    let mut o = [0i32; 16];
    for x in 0..4 {
        let (e0, e1) = (t[x] + t[8 + x], t[x] - t[8 + x]);
        let (e2, e3) = ((t[4 + x] >> 1) - t[12 + x], t[4 + x] + (t[12 + x] >> 1));
        o[x] = (e0 + e3 + 32) >> 6;
        o[4 + x] = (e1 + e2 + 32) >> 6;
        o[8 + x] = (e1 - e2 + 32) >> 6;
        o[12 + x] = (e0 - e3 + 32) >> 6;
    }
    o
}

/// Rounding offset of the deadzone quantiser: a third of a step intra, a sixth inter.
fn deadzone(qbits: u32, intra: bool) -> i32 {
    (1 << qbits) / if intra { 3 } else { 6 }
}

fn quant1(c: i32, mf: i32, f: i32, qbits: u32) -> i32 {
    let l = ((c.abs() * mf + f) >> qbits).min(LEVEL_MAX);
    if c < 0 { -l } else { l }
}

/// Quantises a transformed block at `qp` into `levels` (position 0 left alone when `ac_only`);
/// returns how many are nonzero.
pub(super) fn quant4(
    c: &[i32; 16],
    qp: u8,
    intra: bool,
    ac_only: bool,
    levels: &mut [i32; 16],
) -> u32 {
    let qbits = 15 + u32::from(qp / 6);
    let mf = &QUANT_MF[usize::from(qp % 6)];
    let f = deadzone(qbits, intra);
    let mut nz = 0;
    for i in usize::from(ac_only)..16 {
        levels[i] = quant1(c[i], mf[pos_class(i)], f, qbits);
        nz += u32::from(levels[i] != 0);
    }
    if ac_only {
        levels[0] = 0;
    }
    nz
}

/// Dequantises a block's levels (position 0 too unless `ac_only`, which leaves it 0 for the
/// DC path to fill).
pub(super) fn dequant4(levels: &[i32; 16], qp: u8, ac_only: bool) -> [i32; 16] {
    let v = &DEQUANT_V[usize::from(qp % 6)];
    let shift = u32::from(qp / 6);
    let mut d = [0i32; 16];
    for i in usize::from(ac_only)..16 {
        d[i] = (levels[i] * v[pos_class(i)]) << shift;
    }
    d
}

/// The 4x4 Hadamard transform (rows then columns, no scaling).
fn hadamard4(x: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for y in 0..4 {
        let s = &x[y * 4..][..4];
        let (a, b) = (s[0] + s[1], s[2] + s[3]);
        let (c, d) = (s[0] - s[1], s[2] - s[3]);
        t[y * 4] = a + b;
        t[y * 4 + 1] = a - b;
        t[y * 4 + 2] = c - d;
        t[y * 4 + 3] = c + d;
    }
    let mut o = [0i32; 16];
    for x in 0..4 {
        let (a, b) = (t[x] + t[4 + x], t[8 + x] + t[12 + x]);
        let (c, d) = (t[x] - t[4 + x], t[8 + x] - t[12 + x]);
        o[x] = a + b;
        o[4 + x] = a - b;
        o[8 + x] = c - d;
        o[12 + x] = c + d;
    }
    o
}

/// Intra16x16 luma DC: the 16 blocks' DC coefficients (4x4, raster by block position)
/// through the Hadamard transform, halved, quantised into `levels`; returns the nonzero count.
pub(super) fn quant_luma_dc(dc: &[i32; 16], qp: u8, levels: &mut [i32; 16]) -> u32 {
    let h = hadamard4(dc);
    let qbits = 16 + u32::from(qp / 6);
    let mf = QUANT_MF[usize::from(qp % 6)][0];
    let f = 2 * deadzone(qbits - 1, true);
    let mut nz = 0;
    for (l, &c) in levels.iter_mut().zip(&h) {
        *l = quant1((c + c.signum()) >> 1, mf, f, qbits);
        nz += u32::from(*l != 0);
    }
    nz
}

/// The decoder's Intra16x16 luma DC (8.5.10): the blocks' dequantised DC coefficients.
pub(super) fn dequant_luma_dc(levels: &[i32; 16], qp: u8) -> [i32; 16] {
    let f = hadamard4(levels);
    let scale = 16 * DEQUANT_V[usize::from(qp % 6)][0];
    let q6 = i32::from(qp / 6);
    f.map(|c| {
        if q6 >= 6 {
            (c * scale) << (q6 - 6)
        } else {
            (c * scale + (1 << (5 - q6))) >> (6 - q6)
        }
    })
}

/// Chroma DC: a plane's four DC coefficients (2x2, raster) through the 2x2 Hadamard,
/// quantised into `levels`; returns the nonzero count.
pub(super) fn quant_chroma_dc(dc: &[i32; 4], qp: u8, intra: bool, levels: &mut [i32; 4]) -> u32 {
    let h = hadamard2(dc);
    let qbits = 16 + u32::from(qp / 6);
    let mf = QUANT_MF[usize::from(qp % 6)][0];
    let f = 2 * deadzone(qbits - 1, intra);
    let mut nz = 0;
    for (l, &c) in levels.iter_mut().zip(&h) {
        *l = quant1(c, mf, f, qbits);
        nz += u32::from(*l != 0);
    }
    nz
}

fn hadamard2(x: &[i32; 4]) -> [i32; 4] {
    [
        x[0] + x[1] + x[2] + x[3],
        x[0] - x[1] + x[2] - x[3],
        x[0] + x[1] - x[2] - x[3],
        x[0] - x[1] - x[2] + x[3],
    ]
}

/// The decoder's chroma DC (8.5.11.2): the four blocks' dequantised DC coefficients.
pub(super) fn dequant_chroma_dc(levels: &[i32; 4], qp: u8) -> [i32; 4] {
    let scale = 16 * DEQUANT_V[usize::from(qp % 6)][0];
    hadamard2(levels).map(|c| ((c * scale) << (qp / 6)) >> 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_round_trips_at_low_qp() {
        let r: [i32; 16] = core::array::from_fn(|i| (i as i32 * 37 % 61) - 30);
        let c = fdct4(&r);
        let mut l = [0; 16];
        quant4(&c, 0, true, false, &mut l);
        let back = idct4(&dequant4(&l, 0, false));
        for (a, b) in r.iter().zip(&back) {
            assert!((a - b).abs() <= 1, "{r:?} {back:?}");
        }
    }

    #[test]
    fn flat_blocks_carry_dc_only() {
        let c = fdct4(&[10; 16]);
        assert_eq!(c[0], 160);
        assert!(c[1..].iter().all(|&v| v == 0));
        // Through the Intra16x16 DC path: 16 equal DCs give one Hadamard term.
        let mut l = [0; 16];
        assert_eq!(quant_luma_dc(&[160; 16], 10, &mut l), 1);
        let dc = dequant_luma_dc(&l, 10);
        let mut d = [0; 16];
        d[0] = dc[5];
        let r = idct4(&d);
        assert!(r.iter().all(|&v| (v - 10).abs() <= 1), "{r:?}");
    }

    #[test]
    fn chroma_dc_round_trips() {
        let mut l = [0; 4];
        quant_chroma_dc(&[160, 320, -160, 0], 4, true, &mut l);
        let dc = dequant_chroma_dc(&l, 4);
        for (i, want) in [10, 20, -10, 0].iter().enumerate() {
            let mut d = [0; 16];
            d[0] = dc[i];
            let r = idct4(&d)[0];
            assert!((r - want).abs() <= 1, "{i}: {r}");
        }
    }
}
