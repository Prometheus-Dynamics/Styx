//! Scalar kernels: the correctness oracle every SIMD leaf is tested against, and the tails the
//! leaves leave over. Working values are 12-bit (0..=4095) in `u16`.

use super::{RowKind, YuvCoeffs};
use crate::format::CfaPattern;

/// The largest working value (12 bits).
pub const WORK_MAX: u16 = 4095;

/// CSI-2 packed RAW10: four pixels in five bytes, the first four holding bits 9..2 of each
/// pixel and the fifth their bits 1..0 (pixel 0 in bits 1..0).
pub fn unpack_raw10_row(src: &[u8], dst: &mut [u16], width: usize) {
    for (x, d) in dst[..width].iter_mut().enumerate() {
        let group = x / 4 * 5;
        let lane = x % 4;
        let high = src[group + lane] as u16;
        let low = (src[group + 4] >> (2 * lane)) as u16 & 3;
        *d = high << 2 | low;
    }
}

/// CSI-2 packed RAW12: two pixels in three bytes, the first two holding bits 11..4 and the
/// third their bits 3..0 (pixel 0 in the low nibble).
pub fn unpack_raw12_row(src: &[u8], dst: &mut [u16], width: usize) {
    for (x, d) in dst[..width].iter_mut().enumerate() {
        let pair = x / 2 * 3;
        let lane = x % 2;
        let high = src[pair + lane] as u16;
        let low = (src[pair + 2] >> (4 * lane)) as u16 & 0xF;
        *d = high << 4 | low;
    }
}

/// Black level, gain and clamp in place: `min(((v -sat black) << shift) * gain >> 16, 4095)`.
/// `black[0]` applies to even columns, `black[1]` to odd ones; `gains` holds one Q12 gain per
/// column (with `shift = 16 - bit depth`, a gain of 4096 maps full scale to 4095).
pub fn front_row(row: &mut [u16], black: [u16; 2], gains: &[u16], shift: u32, width: usize) {
    for (x, (v, &g)) in row[..width].iter_mut().zip(&gains[..width]).enumerate() {
        let d = v.saturating_sub(black[x & 1]) << shift;
        *v = ((d as u32 * g as u32) >> 16).min(WORK_MAX as u32) as u16;
    }
}

/// Bilinear demosaic of one row. `up`, `cur` and `dn` hold the rows above, at and below, from
/// column -1 to column `width` (so index `x + 1` is column `x`); outputs are planar.
pub fn demosaic_bilinear_row(rows: [&[u16]; 3], out: [&mut [u16]; 3], width: usize, kind: RowKind) {
    let [up, cur, dn] = rows;
    let [r, g, b] = out;
    for x in 0..width {
        let c = cur[x + 1] as u32;
        let h = cur[x] as u32 + cur[x + 2] as u32;
        let v = up[x + 1] as u32 + dn[x + 1] as u32;
        let (gv, xv, yv) = if kind.is_green(x) {
            (c, (h + 1) >> 1, (v + 1) >> 1)
        } else {
            let d = up[x] as u32 + up[x + 2] as u32 + dn[x] as u32 + dn[x + 2] as u32;
            ((h + v + 2) >> 2, c, (d + 2) >> 2)
        };
        let (rv, bv) = if kind.x_is_red { (xv, yv) } else { (yv, xv) };
        r[x] = rv as u16;
        g[x] = gv as u16;
        b[x] = bv as u16;
    }
}

/// Malvar-He-Cutler gradient-corrected demosaic of one row. `rows` are the five rows from two
/// above to two below, from column -2 to column `width + 1` (index `x + 2` is column `x`).
pub fn demosaic_mhc_row(rows: [&[u16]; 5], out: [&mut [u16]; 3], width: usize, kind: RowKind) {
    let [u2, u1, c0, d1, d2] = rows;
    let [r, g, b] = out;
    for x in 0..width {
        let i = x + 2;
        let p = |row: &[u16], dx: isize| row[(i as isize + dx) as usize] as i32;
        let c = p(c0, 0);
        // Same-colour ring at distance two, and the four cross / diagonal neighbours.
        let far_h = p(c0, -2) + p(c0, 2);
        let far_v = p(u2, 0) + p(d2, 0);
        let near_h = p(c0, -1) + p(c0, 1);
        let near_v = p(u1, 0) + p(d1, 0);
        let diag = p(u1, -1) + p(u1, 1) + p(d1, -1) + p(d1, 1);
        let (gv, xv, yv) = if kind.is_green(x) {
            // Green site: X from the horizontal neighbours, Y from the vertical ones.
            let xs = 10 * c + 8 * near_h - 2 * (far_h + diag) + far_v;
            let ys = 10 * c + 8 * near_v - 2 * (far_v + diag) + far_h;
            (c as u16, mhc_round(xs), mhc_round(ys))
        } else {
            let gs = 8 * c + 4 * (near_h + near_v) - 2 * (far_h + far_v);
            let ys = 12 * c + 4 * diag - 3 * (far_h + far_v);
            (mhc_round(gs), c as u16, mhc_round(ys))
        };
        let (rv, bv) = if kind.x_is_red { (xv, yv) } else { (yv, xv) };
        r[x] = rv;
        g[x] = gv;
        b[x] = bv;
    }
}

/// The MHC kernels sum to 16: `(s + 8) >> 4` clamped to the working range.
#[inline(always)]
pub fn mhc_round(s: i32) -> u16 {
    ((s + 8) >> 4).clamp(0, WORK_MAX as i32) as u16
}

/// Full-resolution luma straight from the mosaic: the 3x3 binomial filter `[1 2 1]^T [1 2 1] / 16`
/// weighs every Bayer position as `(R + 2G + B) / 4`. Rows as in [`demosaic_bilinear_row`].
pub fn bayer_luma_row(rows: [&[u16]; 3], dst: &mut [u16], width: usize) {
    let [up, cur, dn] = rows;
    let vs = |i: usize| up[i] as u32 + 2 * cur[i] as u32 + dn[i] as u32;
    for (x, d) in dst[..width].iter_mut().enumerate() {
        let s = vs(x) + 2 * vs(x + 1) + vs(x + 2);
        *d = ((s + 8) >> 4) as u16;
    }
}

/// One RGB pixel per 2x2 quad of rows `top` and `bottom` (`2 * width` samples each): the red
/// and blue samples, and the rounded mean of the two greens.
pub fn quad_rgb_row(
    top: &[u16],
    bottom: &[u16],
    out: [&mut [u16]; 3],
    width: usize,
    pattern: CfaPattern,
) {
    let [r, g, b] = out;
    for i in 0..width {
        let q = [top[2 * i], top[2 * i + 1], bottom[2 * i], bottom[2 * i + 1]];
        let (rv, gv, bv) = quad_colours(q, pattern);
        r[i] = rv;
        g[i] = gv;
        b[i] = bv;
    }
}

/// R, mean G and B of a quad `[top-left, top-right, bottom-left, bottom-right]`.
#[inline(always)]
pub fn quad_colours(q: [u16; 4], pattern: CfaPattern) -> (u16, u16, u16) {
    let avg = |a: u16, b: u16| ((a as u32 + b as u32 + 1) >> 1) as u16;
    match pattern {
        CfaPattern::Rggb => (q[0], avg(q[1], q[2]), q[3]),
        CfaPattern::Bggr => (q[3], avg(q[1], q[2]), q[0]),
        CfaPattern::Grbg => (q[1], avg(q[0], q[3]), q[2]),
        CfaPattern::Gbrg => (q[2], avg(q[0], q[3]), q[1]),
    }
}

/// Half-resolution luma: the rounded mean of each quad, `(R + 2G + B) / 4`.
pub fn quad_luma_row(top: &[u16], bottom: &[u16], dst: &mut [u16], width: usize) {
    for (i, d) in dst[..width].iter_mut().enumerate() {
        let s = top[2 * i] as u32
            + top[2 * i + 1] as u32
            + bottom[2 * i] as u32
            + bottom[2 * i + 1] as u32;
        *d = ((s + 2) >> 2) as u16;
    }
}

/// One colour-matrix term: `round(x c / 4096)` as the rounding high-half multiply
/// `(8x c + 2^14) >> 15` (x86 `pmulhrsw`, NEON `sqrdmulh`).
#[inline(always)]
pub fn ccm_term(x: u16, c: i16) -> i16 {
    ((((x as i32) << 3) * c as i32 + (1 << 14)) >> 15) as i16
}

/// Colour correction in place with a Q12 matrix (coefficients within ±4, so no term
/// overflows): `out_k = clamp(t(R, m[3k]) +sat t(G, m[3k+1]) +sat t(B, m[3k+2]), 0, 4095)` with
/// [`ccm_term`] and 16-bit saturating adds, on 12-bit inputs.
pub fn ccm_row(planes: [&mut [u16]; 3], m: &[i16; 9], width: usize) {
    let [r, g, b] = planes;
    for x in 0..width {
        let px = [r[x], g[x], b[x]];
        let out = |k: usize| {
            let s = ccm_term(px[0], m[3 * k])
                .saturating_add(ccm_term(px[1], m[3 * k + 1]))
                .saturating_add(ccm_term(px[2], m[3 * k + 2]));
            s.clamp(0, WORK_MAX as i16) as u16
        };
        let (a, c, e) = (out(0), out(1), out(2));
        r[x] = a;
        g[x] = c;
        b[x] = e;
    }
}

/// 12-bit to 8-bit by truncation (saturating, for out-of-range inputs).
pub fn narrow_row(src: &[u16], dst: &mut [u8], width: usize) {
    for (d, &s) in dst[..width].iter_mut().zip(&src[..width]) {
        *d = (s >> 4).min(255) as u8;
    }
}

/// 12-bit to 8-bit through a 4096-entry table (inputs clamped to 4095).
pub fn lut_row(src: &[u16], dst: &mut [u8], lut: &[u8; 4096], width: usize) {
    for (d, &s) in dst[..width].iter_mut().zip(&src[..width]) {
        *d = lut[s.min(WORK_MAX) as usize];
    }
}

/// Planar 8-bit R, G, B to packed RGB24.
pub fn interleave_rgb_row(planes: [&[u8]; 3], dst: &mut [u8], width: usize) {
    let [r, g, b] = planes;
    for (x, px) in dst[..width * 3].chunks_exact_mut(3).enumerate() {
        px[0] = r[x];
        px[1] = g[x];
        px[2] = b[x];
    }
}

/// Luma from planar 8-bit RGB: `min(((cr R + cg G + cb B + 128) >> 8) + offset, 255)`.
pub fn rgb_to_y_row(planes: [&[u8]; 3], dst: &mut [u8], width: usize, c: &YuvCoeffs) {
    let [r, g, b] = planes;
    for (x, d) in dst[..width].iter_mut().enumerate() {
        let s =
            c.y[0] as u32 * r[x] as u32 + c.y[1] as u32 * g[x] as u32 + c.y[2] as u32 * b[x] as u32;
        *d = (((s + 128) >> 8) + c.y_offset as u32).min(255) as u8;
    }
}

/// The rounded mean of a 2x2 block of one channel.
#[inline(always)]
fn mean4(top: &[u8], bottom: &[u8], i: usize) -> i32 {
    (top[2 * i] as i32
        + top[2 * i + 1] as i32
        + bottom[2 * i] as i32
        + bottom[2 * i + 1] as i32
        + 2)
        >> 2
}

/// One chroma component from a Q7 row of coefficients.
#[inline(always)]
pub fn chroma(k: &[i16; 3], r: i32, g: i32, b: i32) -> u8 {
    let s = k[0] as i32 * r + k[1] as i32 * g + k[2] as i32 * b;
    (128 + ((s + 64) >> 7)).clamp(0, 255) as u8
}

/// 4:2:0 chroma from two rows of planar 8-bit RGB (`2 * width` pixels each): each 2x2 block is
/// averaged, then converted. With `interleaved`, `u` receives `width` UV pairs (NV12) and `v`
/// is unused; otherwise `u` and `v` get `width` samples each.
pub fn rgb_to_uv_row(
    top: [&[u8]; 3],
    bottom: [&[u8]; 3],
    u: &mut [u8],
    v: &mut [u8],
    width: usize,
    c: &YuvCoeffs,
    interleaved: bool,
) {
    for i in 0..width {
        let r = mean4(top[0], bottom[0], i);
        let g = mean4(top[1], bottom[1], i);
        let b = mean4(top[2], bottom[2], i);
        let (cu, cv) = (chroma(&c.u, r, g, b), chroma(&c.v, r, g, b));
        if interleaved {
            u[2 * i] = cu;
            u[2 * i + 1] = cv;
        } else {
            u[i] = cu;
            v[i] = cv;
        }
    }
}

/// Sums over quads (R, G, B rows of `width`): `[R, G, B, count]` of the quads whose channels
/// are all below `sat`, and the sum of every quad's luma `(R + 2G + B + 2) >> 2`.
pub fn zone_sums(rgb: [&[u16]; 3], width: usize, sat: u16) -> [u32; 5] {
    let mut s = [0u32; 5];
    let [r, g, b] = rgb.map(|p| &p[..width]);
    for ((&r, &g), &b) in r.iter().zip(g).zip(b) {
        let (r, g, b) = (r as u32, g as u32, b as u32);
        let keep = (r.max(g).max(b) < sat as u32) as u32;
        s[0] += r * keep;
        s[1] += g * keep;
        s[2] += b * keep;
        s[3] += keep;
        s[4] += (r + 2 * g + b + 2) >> 2;
    }
    s
}
