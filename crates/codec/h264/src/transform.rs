//! Quantisers and scales for the 4x4 kernels (`styx_simd::h264`), and the DC transforms
//! of Intra16x16 luma and chroma with their quantisation, as the decoder inverts them
//! (8.5.10, 8.5.11.2 with flat scaling lists).

use styx_simd::h264::{Quant4, quant1};

use crate::tables::{DEQUANT_V, QUANT_MF, pos_class};

/// Rounding offset of the deadzone quantiser: a third of a step intra, a sixth inter.
fn deadzone(qbits: u32, intra: bool) -> i32 {
    (1 << qbits) / if intra { 3 } else { 6 }
}

/// The 4x4 quantiser at `qp` for `simd::h264::residual_quant`.
pub(super) fn quantiser(qp: u8, intra: bool) -> Quant4 {
    let qbits = 15 + u32::from(qp / 6);
    let mf = &QUANT_MF[usize::from(qp % 6)];
    Quant4 {
        mf: core::array::from_fn(|i| mf[pos_class(i)]),
        f: deadzone(qbits, intra),
        qbits,
    }
}

/// The dequantisation scale per position at `qp` for `simd::h264::recon`.
pub(super) fn scale(qp: u8) -> [i32; 16] {
    let v = &DEQUANT_V[usize::from(qp % 6)];
    core::array::from_fn(|i| v[pos_class(i)] << (qp / 6))
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
    use styx_simd::h264::{fdct4, idct4};

    use super::*;

    #[test]
    fn transform_round_trips_at_low_qp() {
        let r: [i32; 16] = core::array::from_fn(|i| (i as i32 * 37 % 61) - 30);
        let c = fdct4(&r);
        let q = quantiser(0, true);
        let sc = scale(0);
        let d: [i32; 16] = core::array::from_fn(|i| quant1(c[i], q.mf[i], q.f, q.qbits) * sc[i]);
        let back = idct4(&d);
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
