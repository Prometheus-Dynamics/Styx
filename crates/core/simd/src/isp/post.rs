//! Row kernels of the output picture's stages (the `post` module of the soft ISP): RGB to and from JFIF YCbCr
//! planes, the luma and chroma denoise filters and the unsharp mask. As in the parent module,
//! the scalar kernels are the oracle and each leaf must produce exactly their result.
//!
//! The three filters read a 3x3 neighbourhood, so their leaves cover the interior of a row:
//! `dst` is the columns `1 .. w - 1` of rows of `w = dst.len() + 2` samples. The first and last
//! column mirror their missing neighbour and take the scalar path.

use super::{SimdBackend, finish};
#[allow(unused_imports)]
use super::{X86FeatureSet, leaf};

#[cfg(all(feature = "neon", target_arch = "aarch64"))]
use super::neon;
#[cfg(all(
    feature = "neon",
    feature = "std",
    target_arch = "arm",
    target_os = "linux"
))]
use super::neon32;
#[cfg(all(feature = "rvv", target_arch = "riscv64", target_os = "linux"))]
use super::rvv;
#[cfg(all(feature = "x86", any(target_arch = "x86", target_arch = "x86_64")))]
use super::x86;

/// `round(65536 / n)` for the counts of the 3x3 sigma filter (1 to 9), in 16 bits: the mean of
/// `n` values with sum `s` is `(s RCP[n] + 2^15) >> 16`, exact for a flat field. `n = 1` holds
/// 65535 for 65536: `(c 65535 + 2^15) >> 16` is still `c` for every 8-bit `c`.
pub const RCP: [u16; 10] = [
    0, 65535, 32768, 21845, 16384, 13107, 10923, 9362, 8192, 7282,
];

/// The columns either side of `x` in a row of `w`, the missing one mirrored.
#[inline(always)]
fn sides(x: usize, w: usize) -> (usize, usize) {
    let left = if x == 0 { 1.min(w - 1) } else { x - 1 };
    let right = if x + 1 == w {
        w.saturating_sub(2)
    } else {
        x + 1
    };
    (left, right)
}

/// A row by `px` per column, the interior's prefix from `inner` when it has a leaf.
fn over_row(
    dst: &mut [u8],
    px: impl Fn(usize) -> u8,
    inner: impl FnOnce(&mut [u8]) -> Option<(SimdBackend, usize)>,
) -> SimdBackend {
    let w = dst.len();
    if w == 0 {
        return SimdBackend::Scalar;
    }
    dst[0] = px(0);
    dst[w - 1] = px(w - 1);
    if w < 3 {
        return SimdBackend::Scalar;
    }
    let body = &mut dst[1..w - 1];
    let outcome = inner(body);
    finish(outcome, w - 2, |d, n| {
        for (i, s) in body[d..d + n].iter_mut().enumerate() {
            *s = px(d + i + 1);
        }
    })
}

/// One sample of the sigma filter at column `x`.
#[inline(always)]
fn sigma_at(rows: [&[u8]; 3], x: usize, table: &[u16; 256], s: i16) -> u8 {
    let [up, cur, dn] = rows;
    let (l, r) = sides(x, cur.len());
    let c = i32::from(cur[x]);
    let t = i32::from(table[usize::from(cur[x])]);
    let (mut sum, mut cnt) = (c, 1usize);
    for v in [up[l], up[x], up[r], cur[l], cur[r], dn[l], dn[x], dn[r]] {
        let v = i32::from(v);
        let ok = (v - c).abs() * 4 <= t;
        sum += i32::from(ok) * v;
        cnt += usize::from(ok);
    }
    let avg = (sum * i32::from(RCP[cnt]) + (1 << 15)) >> 16;
    (c + (((avg - c) * i32::from(s) + 128) >> 8)).clamp(0, 255) as u8
}

/// One sample of the chroma filter at column `x`.
#[inline(always)]
fn chroma_at(rows: [&[u8]; 3], x: usize, t: i16, s: i16) -> u8 {
    let [up, cur, dn] = rows;
    let (l, r) = sides(x, cur.len());
    let vsum = |x: usize| i32::from(up[x]) + 2 * i32::from(cur[x]) + i32::from(dn[x]);
    let c = i32::from(cur[x]);
    let low = (vsum(l) + 2 * vsum(x) + vsum(r) + 8) >> 4;
    let ok = i32::from((low - c).abs() * 4 <= i32::from(t));
    (c + ok * (((low - c) * i32::from(s) + 128) >> 8)).clamp(0, 255) as u8
}

/// One sample of the unsharp mask at column `x`.
#[inline(always)]
fn sharpen_at(rows: [&[u8]; 3], x: usize, sharpen: Sharpening) -> u8 {
    let [up, cur, dn] = rows;
    let (l, r) = sides(x, cur.len());
    let c = i32::from(cur[x]);
    let vsum = |x: usize| i32::from(up[x]) + 2 * i32::from(cur[x]) + i32::from(dn[x]);
    let blur = (vsum(l) + 2 * vsum(x) + vsum(r) + 8) >> 4;
    let detail = c - blur;
    let cored = detail.signum() * (detail.abs() - sharpen.threshold).max(0);
    let limit = sharpen.limit;
    let corr = ((sharpen.amount * cored + 128) >> 8).clamp(-limit, limit);
    (c + corr).clamp(0, 255) as u8
}

/// The unsharp mask's parameters: amount in Q8, threshold and limit in codes.
#[derive(Clone, Copy, Debug)]
pub struct Sharpening {
    pub amount: i32,
    pub threshold: i32,
    pub limit: i32,
}

/// Scalar kernels: the oracle of the leaves below.
pub mod scalar {
    #[cfg(test)]
    use super::*;

    /// Interleaved RGB to JFIF `Y`, `Cb`, `Cr` planes.
    pub fn rgb_to_planes_row(rgb: &[u8], y: &mut [u8], cb: &mut [u8], cr: &mut [u8], width: usize) {
        for (i, px) in rgb[..3 * width].as_chunks::<3>().0.iter().enumerate() {
            let (r, g, b) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
            y[i] = ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(0, 255) as u8;
            cb[i] = (((-43 * r - 85 * g + 128 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            cr[i] = (((128 * r - 107 * g - 21 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }

    /// JFIF `Y`, `Cb`, `Cr` planes to planar RGB, rounded and clamped.
    pub fn planes_to_rgb_row(planes: [&[u8]; 3], rgb: [&mut [u8]; 3], width: usize) {
        let [ys, cbs, crs] = planes;
        let [r, g, b] = rgb;
        for x in 0..width {
            let (y, cb, cr) = (
                i32::from(ys[x]),
                i32::from(cbs[x]) - 128,
                i32::from(crs[x]) - 128,
            );
            r[x] = ((256 * y + 359 * cr + 128) >> 8).clamp(0, 255) as u8;
            g[x] = ((256 * y - 88 * cb - 183 * cr + 128) >> 8).clamp(0, 255) as u8;
            b[x] = ((256 * y + 454 * cb + 128) >> 8).clamp(0, 255) as u8;
        }
    }

    /// The sigma filter of a whole row; `table` the threshold (quarter codes) of each centre
    /// value, `s` the strength in Q8 (at most 256).
    #[cfg(test)]
    pub fn sigma_row(rows: [&[u8]; 3], dst: &mut [u8], table: &[u16; 256], s: i16) {
        let rows = rows.map(|r| &r[..dst.len()]);
        over_row(dst, |x| sigma_at(rows, x, table, s), |_| None);
    }

    /// The chroma filter of a whole row; threshold `t` in quarter codes (at most 1020),
    /// strength `s` in Q8 (at most 256).
    #[cfg(test)]
    pub fn chroma_row(rows: [&[u8]; 3], dst: &mut [u8], t: i16, s: i16) {
        let rows = rows.map(|r| &r[..dst.len()]);
        over_row(dst, |x| chroma_at(rows, x, t, s), |_| None);
    }

    /// The unsharp mask of a whole row.
    #[cfg(test)]
    pub fn sharpen_row(rows: [&[u8]; 3], dst: &mut [u8], sharpen: Sharpening) {
        let rows = rows.map(|r| &r[..dst.len()]);
        over_row(dst, |x| sharpen_at(rows, x, sharpen), |_| None);
    }
}

/// See [`scalar::rgb_to_planes_row`].
pub fn rgb_to_planes_row(
    rgb: &[u8],
    y: &mut [u8],
    cb: &mut [u8],
    cr: &mut [u8],
    width: usize,
) -> SimdBackend {
    let rgb = &rgb[..3 * width];
    let (y, cb, cr) = (&mut y[..width], &mut cb[..width], &mut cr[..width]);
    let outcome = leaf!(rgb_to_planes_row(rgb, &mut *y, &mut *cb, &mut *cr, width));
    finish(outcome, width, |d, n| {
        scalar::rgb_to_planes_row(&rgb[3 * d..], &mut y[d..], &mut cb[d..], &mut cr[d..], n)
    })
}

/// See [`scalar::planes_to_rgb_row`].
pub fn planes_to_rgb_row(planes: [&[u8]; 3], rgb: [&mut [u8]; 3], width: usize) -> SimdBackend {
    let planes = planes.map(|p| &p[..width]);
    let [r, g, b] = rgb;
    let (r, g, b) = (&mut r[..width], &mut g[..width], &mut b[..width]);
    let outcome = leaf!(planes_to_rgb_row(
        planes,
        [&mut *r, &mut *g, &mut *b],
        width
    ));
    finish(outcome, width, |d, n| {
        scalar::planes_to_rgb_row(
            planes.map(|p| &p[d..]),
            [&mut r[d..], &mut g[d..], &mut b[d..]],
            n,
        )
    })
}

/// The sigma filter of a whole row; `table` the threshold (quarter codes) of each centre value,
/// `s` the strength in Q8 (at most 256).
pub fn sigma_row(rows: [&[u8]; 3], dst: &mut [u8], table: &[u16; 256], s: i16) -> SimdBackend {
    let rows = rows.map(|r| &r[..dst.len()]);
    over_row(
        dst,
        |x| sigma_at(rows, x, table, s),
        |body| {
            let _ = &body;
            leaf!(sigma_row(rows, body, table, s))
        },
    )
}

/// The chroma filter of a whole row; threshold `t` in quarter codes (at most 1020), strength `s`
/// in Q8 (at most 256).
pub fn chroma_row(rows: [&[u8]; 3], dst: &mut [u8], t: i16, s: i16) -> SimdBackend {
    let rows = rows.map(|r| &r[..dst.len()]);
    over_row(
        dst,
        |x| chroma_at(rows, x, t, s),
        |body| {
            let _ = &body;
            leaf!(chroma_row(rows, body, t, s))
        },
    )
}

/// The unsharp mask of a whole row. An amount past 16 bits (127 times the detail, far past any
/// limit) stays scalar; a threshold over 256 or a limit over 255 acts as those values.
pub fn sharpen_row(rows: [&[u8]; 3], dst: &mut [u8], sharpen: Sharpening) -> SimdBackend {
    let rows = rows.map(|r| &r[..dst.len()]);
    let (amount, thr, limit) = (
        sharpen.amount,
        sharpen.threshold.min(256) as i16,
        sharpen.limit.min(255) as i16,
    );
    over_row(
        dst,
        |x| sharpen_at(rows, x, sharpen),
        |body| {
            let _ = (&body, thr, limit);
            if amount > i32::from(i16::MAX) {
                return None;
            }
            leaf!(sharpen_row(rows, body, amount as i16, thr, limit))
        },
    )
}
