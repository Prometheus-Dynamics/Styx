//! The arithmetic of an H.264 encoder's hot loops, on grids of 4x4 blocks: SAD for motion
//! search, the residual's forward core transform with deadzone quantisation, and the
//! decoder's reconstruction (dequantisation with flat scaling lists, the inverse core
//! transform rounded, prediction added and clamped, 8.5.12). The scalar functions here are
//! the oracle; every vector leaf (RVV) gives the same numbers.
//!
//! Blocks are in raster order over the grid (block `by * bw + bx`), samples and coefficients
//! in raster order within a block.

use super::SimdBackend;

mod hpel;
pub use hpel::*;

/// The largest level magnitude produced: CAVLC in Baseline codes up to here at any suffix
/// length.
pub const LEVEL_MAX: i32 = 2047;

/// A quantiser: `level = sign(c) · min((|c| · mf[i] + f) >> qbits, LEVEL_MAX)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quant4 {
    /// Multiplier per raster position.
    pub mf: [i32; 16],
    /// Rounding offset (deadzone).
    pub f: i32,
    /// Shift, 15 to 23.
    pub qbits: u32,
}

/// The forward core transform of a 4x4 residual.
pub fn fdct4(r: &[i32; 16]) -> [i32; 16] {
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

/// The inverse core transform of dequantised coefficients, rounded (`(x + 32) >> 6`).
pub fn idct4(d: &[i32; 16]) -> [i32; 16] {
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

/// One coefficient quantised.
#[inline]
pub fn quant1(c: i32, mf: i32, f: i32, qbits: u32) -> i32 {
    let l = ((c.abs() * mf + f) >> qbits).min(LEVEL_MAX);
    if c < 0 { -l } else { l }
}

/// SAD of two 16x16 blocks (rows `a_stride` and `b_stride` apart).
pub fn sad16(a: &[u8], a_stride: usize, b: &[u8], b_stride: usize) -> u32 {
    assert!(a.len() >= 15 * a_stride + 16 && b.len() >= 15 * b_stride + 16);
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if let Some(s) = super::rvv::h264::sad16(a, a_stride, b, b_stride) {
        return s;
    }
    sad16_scalar(a, a_stride, b, b_stride)
}

/// The scalar SAD.
pub fn sad16_scalar(a: &[u8], a_stride: usize, b: &[u8], b_stride: usize) -> u32 {
    let mut s = 0;
    for r in 0..16 {
        let (x, y) = (&a[r * a_stride..][..16], &b[r * b_stride..][..16]);
        s += x
            .iter()
            .zip(y)
            .map(|(p, q)| u32::from(p.abs_diff(*q)))
            .sum::<u32>();
    }
    s
}

fn check_grid(plane: &[u8], stride: usize, (bw, bh): (usize, usize)) {
    assert!(bw > 0 && bh > 0 && stride >= 4 * bw);
    assert!(
        plane.len() >= (4 * bh - 1) * stride + 4 * bw,
        "plane too short"
    );
}

/// Residual (`src − pred`) of a `bw` x `bh` grid of 4x4 blocks, transformed and quantised:
/// `dc[b]` the unquantised DC coefficient of block `b`, `levels[b]` its levels (position 0
/// left at zero when `ac_only`, for the DC transforms of Intra16x16 and chroma).
#[allow(clippy::too_many_arguments)]
pub fn residual_quant(
    src: &[u8],
    src_stride: usize,
    pred: &[u8],
    pred_stride: usize,
    grid: (usize, usize),
    q: &Quant4,
    ac_only: bool,
    dc: &mut [i32],
    levels: &mut [[i32; 16]],
) -> SimdBackend {
    check_grid(src, src_stride, grid);
    check_grid(pred, pred_stride, grid);
    let n = grid.0 * grid.1;
    assert!(dc.len() >= n && levels.len() >= n && (15..=23).contains(&q.qbits));
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if let Some(b) = super::rvv::h264::residual_quant(
        src,
        src_stride,
        pred,
        pred_stride,
        grid,
        q,
        ac_only,
        dc,
        levels,
    ) {
        return b;
    }
    residual_quant_scalar(
        src,
        src_stride,
        pred,
        pred_stride,
        grid,
        q,
        ac_only,
        dc,
        levels,
    );
    SimdBackend::Scalar
}

/// The scalar [`residual_quant`].
#[allow(clippy::too_many_arguments)]
pub fn residual_quant_scalar(
    src: &[u8],
    src_stride: usize,
    pred: &[u8],
    pred_stride: usize,
    (bw, bh): (usize, usize),
    q: &Quant4,
    ac_only: bool,
    dc: &mut [i32],
    levels: &mut [[i32; 16]],
) {
    for b in 0..bw * bh {
        let (x, y) = (b % bw * 4, b / bw * 4);
        let r: [i32; 16] = core::array::from_fn(|i| {
            let (c, rr) = (x + (i & 3), y + (i >> 2));
            i32::from(src[rr * src_stride + c]) - i32::from(pred[rr * pred_stride + c])
        });
        let c = fdct4(&r);
        dc[b] = c[0];
        for (i, l) in levels[b].iter_mut().enumerate() {
            *l = if i == 0 && ac_only {
                0
            } else {
                quant1(c[i], q.mf[i], q.f, q.qbits)
            };
        }
    }
}

/// The decoder's reconstruction of a `bw` x `bh` grid: each block's `levels` times `scale`
/// (per raster position, `normAdjust << (qp / 6)`), position 0 replaced by `dc[b]` when
/// given, inverse transformed, added to `pred` and clamped into `dst`.
#[allow(clippy::too_many_arguments)]
pub fn recon(
    levels: &[[i32; 16]],
    scale: &[i32; 16],
    dc: Option<&[i32]>,
    pred: &[u8],
    pred_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    grid: (usize, usize),
) -> SimdBackend {
    check_grid(pred, pred_stride, grid);
    check_grid(dst, dst_stride, grid);
    let n = grid.0 * grid.1;
    assert!(levels.len() >= n && dc.is_none_or(|d| d.len() >= n));
    #[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
    if let Some(b) =
        super::rvv::h264::recon(levels, scale, dc, pred, pred_stride, dst, dst_stride, grid)
    {
        return b;
    }
    recon_scalar(levels, scale, dc, pred, pred_stride, dst, dst_stride, grid);
    SimdBackend::Scalar
}

/// The scalar [`recon`].
#[allow(clippy::too_many_arguments)]
pub fn recon_scalar(
    levels: &[[i32; 16]],
    scale: &[i32; 16],
    dc: Option<&[i32]>,
    pred: &[u8],
    pred_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    (bw, bh): (usize, usize),
) {
    for b in 0..bw * bh {
        let (x, y) = (b % bw * 4, b / bw * 4);
        let mut d: [i32; 16] = core::array::from_fn(|i| levels[b][i] * scale[i]);
        if let Some(dc) = dc {
            d[0] = dc[b];
        }
        let r = idct4(&d);
        for (i, &v) in r.iter().enumerate() {
            let (c, rr) = (x + (i & 3), y + (i >> 2));
            let p = i32::from(pred[rr * pred_stride + c]);
            dst[rr * dst_stride + c] = (p + v).clamp(0, 255) as u8;
        }
    }
}

/// The most whole pels [`full_search`] (and the GPU's motion search) looks either way.
pub const SEARCH_RANGE_MAX: i32 = 16;

/// Exhaustive whole-pel motion search of the 16x16 macroblock (`mbx`, `mby`) of `cur` in
/// `reference` (both `w` x `h` with rows `w` apart, `w` and `h` multiples of 16), offsets up
/// to `range` either way with the block inside the picture: the offset (whole pels) with the
/// least SAD, ties to the least `|dx| + |dy|`, then the first in raster order of the
/// `(2 range + 1)²` candidates. Returns the offset and its SAD.
pub fn full_search(
    cur: &[u8],
    reference: &[u8],
    (w, h): (usize, usize),
    (mbx, mby): (usize, usize),
    range: i32,
) -> ([i32; 2], u32) {
    assert!(w % 16 == 0 && h % 16 == 0 && cur.len() >= w * h && reference.len() >= w * h);
    assert!((0..=SEARCH_RANGE_MAX).contains(&range));
    let (x0, y0) = (mbx as i32 * 16, mby as i32 * 16);
    let mut best = (u32::MAX, u32::MAX, [0, 0]);
    let d = 2 * range + 1;
    for c in 0..d * d {
        let (dx, dy) = (c % d - range, c / d - range);
        let (rx, ry) = (x0 + dx, y0 + dy);
        if rx < 0 || ry < 0 || rx + 16 > w as i32 || ry + 16 > h as i32 {
            continue;
        }
        let sad = sad16_scalar(
            &cur[y0 as usize * w + x0 as usize..],
            w,
            &reference[ry as usize * w + rx as usize..],
            w,
        );
        let tie = dx.unsigned_abs() + dy.unsigned_abs();
        if (sad, tie) < (best.0, best.1) {
            best = (sad, tie, [dx, dy]);
        }
    }
    (best.2, best.0)
}
