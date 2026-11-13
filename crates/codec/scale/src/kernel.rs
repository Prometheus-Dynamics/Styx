//! The per-sample arithmetic of the software path, and its scalar reference.
//!
//! [`Path::Scalar`] computes every output sample straight from the formula in the module's
//! documentation, with no tables and no SIMD. [`Path::Fast`] computes the same numbers, and uses
//! `styx-simd` where a kernel fits exactly: a 2:1 area average of a single-channel plane, and the
//! red and blue swap between RG24 and BG24. Both paths are checked against each other in the tests.
//!
//! The formulas, for an output sample at `(x, y)`:
//!
//! - Area: the input block `[x0, x1) x [y0, y1)` with `x0 = x * sw / dw` and
//!   `x1 = max((x + 1) * sw / dw, x0 + 1)` (integer division, likewise for `y`). The value is
//!   `(sum + n / 2) / n` over the `n` samples of the block, per channel.
//! - Bilinear: the centre of output pixel `x` maps to `p = ((2x + 1) * sw * 256) / (2 dw) - 128`
//!   in 1/256 pixels. `x0 = max(p, 0) >> 8`, `fx = max(p, 0) & 255`, `x1 = min(x0 + 1, sw - 1)`,
//!   and likewise for `y`. With the four corner samples `a, b, c, d` (top-left, top-right,
//!   bottom-left, bottom-right) the value is
//!   `((a (256 - fx) + b fx)(256 - fy) + (c (256 - fx) + d fx) fy + 32768) >> 16`.

use styx_simd::{box2_row, swap_rb24_row};

use crate::layout::{Filter, Rgb};

/// Which implementation runs the arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// The formulas with `styx-simd` rows where they fit.
    Fast,
    /// The formulas, one sample at a time.
    Scalar,
}

/// Resamples a `sw` x `sh` plane of `ch`-byte samples, its rows `src_stride` bytes apart, into
/// `dst`, a packed `dw` x `dh` plane of `ch`-byte samples. `dst` must hold `dw * dh * ch` bytes
/// and `src` the rows named by `sw`, `sh` and `src_stride`.
#[allow(clippy::too_many_arguments)]
pub fn resample(
    path: Path,
    filter: Filter,
    src: &[u8],
    src_stride: usize,
    (sw, sh): (usize, usize),
    ch: usize,
    dst: &mut [u8],
    (dw, dh): (usize, usize),
) {
    debug_assert!(sw > 0 && sh > 0 && dw > 0 && dh > 0);
    debug_assert!(dst.len() >= dw * dh * ch);
    match filter {
        Filter::Area => area(path, src, src_stride, (sw, sh), ch, dst, (dw, dh)),
        Filter::Bilinear => bilinear(src, src_stride, (sw, sh), ch, dst, (dw, dh)),
    }
}

fn area(
    path: Path,
    src: &[u8],
    src_stride: usize,
    (sw, sh): (usize, usize),
    ch: usize,
    dst: &mut [u8],
    (dw, dh): (usize, usize),
) {
    // The 2:1 case of one channel is the 2x2 box row kernel, which rounds the same way.
    if path == Path::Fast && ch == 1 && sw == 2 * dw && sh == 2 * dh {
        for y in 0..dh {
            let top = &src[2 * y * src_stride..][..2 * dw];
            let bottom = &src[(2 * y + 1) * src_stride..][..2 * dw];
            box2_row(top, bottom, &mut dst[y * dw..][..dw], dw);
        }
        return;
    }
    for y in 0..dh {
        let (y0, y1) = block(y, sh, dh);
        for x in 0..dw {
            let (x0, x1) = block(x, sw, dw);
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            for c in 0..ch {
                let mut sum = 0u32;
                for yy in y0..y1 {
                    let row = &src[yy * src_stride..];
                    for xx in x0..x1 {
                        sum += u32::from(row[xx * ch + c]);
                    }
                }
                dst[(y * dw + x) * ch + c] = ((sum + n / 2) / n) as u8;
            }
        }
    }
}

/// The input block `[i0, i1)` of output index `i` on an axis `s` long resized to `d`.
fn block(i: usize, s: usize, d: usize) -> (usize, usize) {
    let i0 = i * s / d;
    let i1 = ((i + 1) * s / d).max(i0 + 1);
    (i0, i1)
}

/// The fixed-point source position of output index `i` on an axis `s` long resized to `d`:
/// the integer sample below, and the 1/256 fraction toward the next one.
fn tap(i: usize, s: usize, d: usize) -> (usize, usize, u32) {
    let p = ((2 * i + 1) * s * 256) as isize / (2 * d) as isize - 128;
    if p <= 0 {
        return (0, 0, 0);
    }
    let p = p as usize;
    let i0 = (p >> 8).min(s - 1);
    let i1 = (i0 + 1).min(s - 1);
    (i0, i1, (p & 255) as u32)
}

fn bilinear(
    src: &[u8],
    src_stride: usize,
    (sw, sh): (usize, usize),
    ch: usize,
    dst: &mut [u8],
    (dw, dh): (usize, usize),
) {
    for y in 0..dh {
        let (y0, y1, fy) = tap(y, sh, dh);
        for x in 0..dw {
            let (x0, x1, fx) = tap(x, sw, dw);
            for c in 0..ch {
                let at = |yy: usize, xx: usize| u32::from(src[yy * src_stride + xx * ch + c]);
                let (a, b) = (at(y0, x0), at(y0, x1));
                let (cc, d) = (at(y1, x0), at(y1, x1));
                let top = a * (256 - fx) + b * fx;
                let bottom = cc * (256 - fx) + d * fx;
                let v = (top * (256 - fy) + bottom * fy + 32768) >> 16;
                dst[(y * dw + x) * ch + c] = v as u8;
            }
        }
    }
}

/// Converts `width` pixels of `src` (kind `from`) into `dst` (kind `to`), both packed.
pub fn convert_pixels(path: Path, from: Rgb, to: Rgb, src: &[u8], dst: &mut [u8], width: usize) {
    if from == to {
        dst[..width * from.bpp()].copy_from_slice(&src[..width * from.bpp()]);
        return;
    }
    let both_24 = from.bpp() == 3 && to.bpp() == 3;
    if path == Path::Fast && both_24 {
        // RG24 and BG24 differ only in the red and blue bytes.
        swap_rb24_row(src, dst, width);
        return;
    }
    let [fr, fg, fb] = from.rgb_index();
    let [tr, tg, tb] = to.rgb_index();
    for x in 0..width {
        let s = &src[x * from.bpp()..][..from.bpp()];
        let d = &mut dst[x * to.bpp()..][..to.bpp()];
        d[tr] = s[fr];
        d[tg] = s[fg];
        d[tb] = s[fb];
        if to == Rgb::Bgra32 {
            d[3] = if from == Rgb::Bgra32 { s[3] } else { 255 };
        }
    }
}
