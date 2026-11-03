//! Quarter-pel luma motion compensation (8.4.2.2.1): the reference's half-pel planes, made
//! once a picture, and predictions from them (whole and half positions read, quarter
//! positions the rounded-up mean of two).
//!
//! The planes cover the picture and [`PAD`] samples around it, every sample from the
//! reference with its coordinates clamped as the decoder clamps them; blocks reaching
//! further take the per-sample path ([`sample`]), which is also the tests' oracle.

use super::picture::Plane;

/// Samples of the half-pel planes beyond each edge.
pub(super) const PAD: usize = 8;

const TAPS: [i32; 6] = [1, -5, 20, 20, -5, 1];

/// A plane and the offset of a block in it.
type Read<'a> = (&'a Vec<u8>, usize);

fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// The reference's full-pel samples (`g`) and its half-pel planes: `b` between (x, y) and
/// (x + 1, y), `h` between (x, y) and (x, y + 1), `j` in the middle of the four; all
/// `stride` wide, (x, y) at `(y + PAD) * stride + x + PAD`.
#[derive(Debug, Default)]
pub(super) struct HalfPel {
    g: Vec<u8>,
    b: Vec<u8>,
    h: Vec<u8>,
    j: Vec<u8>,
    /// The horizontal filter's unrounded sums on the picture's rows.
    b1: Vec<i32>,
    row: Vec<i32>,
    stride: usize,
    size: (usize, usize),
}

impl HalfPel {
    /// Makes the planes from `rf` (allocating only when its size changes).
    pub(super) fn build(&mut self, rf: &Plane) {
        let (w, h) = (rf.w, rf.h);
        let stride = w + 2 * PAD;
        let rows = h + 2 * PAD;
        self.size = (w, h);
        self.stride = stride;
        for p in [&mut self.g, &mut self.b, &mut self.h, &mut self.j] {
            p.resize(stride * rows, 0);
        }
        self.b1.resize(stride * h, 0);
        self.row.resize(stride + 5, 0);
        let clamp_row = |y: isize| y.clamp(0, h as isize - 1) as usize;
        // Full-pel rows, padded both ways.
        for y in 0..rows {
            let src = &rf.data[clamp_row(y as isize - PAD as isize) * w..][..w];
            let dst = &mut self.g[y * stride..][..stride];
            dst[..PAD].fill(src[0]);
            dst[PAD..PAD + w].copy_from_slice(src);
            dst[PAD + w..].fill(src[w - 1]);
        }
        // Horizontal sums on the picture's rows (columns clamped once more for the taps
        // reaching past the padding).
        for y in 0..h {
            let g = &self.g[(y + PAD) * stride..][..stride];
            let row = &mut self.row;
            row[..2].fill(i32::from(g[0]));
            for (r, &v) in row[2..2 + stride].iter_mut().zip(g) {
                *r = i32::from(v);
            }
            row[2 + stride..].fill(i32::from(g[stride - 1]));
            let out = &mut self.b1[y * stride..][..stride];
            for (x, o) in out.iter_mut().enumerate() {
                *o = TAPS.iter().zip(&row[x..x + 6]).map(|(t, v)| t * v).sum();
            }
        }
        for y in 0..rows {
            let yc = y as isize - PAD as isize;
            let b1 = &self.b1[clamp_row(yc) * stride..][..stride];
            for (o, &s) in self.b[y * stride..][..stride].iter_mut().zip(b1) {
                *o = clip((s + 16) >> 5);
            }
            // Vertical taps: rows yc - 2 to yc + 3, clamped.
            let taps: [usize; 6] = core::array::from_fn(|k| clamp_row(yc - 2 + k as isize));
            let grow = |k: usize| &self.g[(taps[k] + PAD) * stride..][..stride];
            let (g0, g1, g2, g3, g4, g5) = (grow(0), grow(1), grow(2), grow(3), grow(4), grow(5));
            let hrow = &mut self.h[y * stride..][..stride];
            for x in 0..stride {
                let s = i32::from(g0[x]) - 5 * i32::from(g1[x])
                    + 20 * i32::from(g2[x])
                    + 20 * i32::from(g3[x])
                    - 5 * i32::from(g4[x])
                    + i32::from(g5[x]);
                hrow[x] = clip((s + 16) >> 5);
            }
            let brow = |k: usize| &self.b1[taps[k] * stride..][..stride];
            let (b0, b1r, b2, b3, b4, b5) = (brow(0), brow(1), brow(2), brow(3), brow(4), brow(5));
            let jrow = &mut self.j[y * stride..][..stride];
            for x in 0..stride {
                let s = b0[x] - 5 * b1r[x] + 20 * b2[x] + 20 * b3[x] - 5 * b4[x] + b5[x];
                jrow[x] = clip((s + 512) >> 10);
            }
        }
    }

    /// The luma prediction of the 16x16 block at (`x`, `y`) moved by `mv` (quarter pels).
    pub(super) fn predict(
        &self,
        rf: &Plane,
        (x, y): (usize, usize),
        mv: [i32; 2],
        out: &mut [u8; 256],
    ) {
        let (fx, fy) = ((mv[0] & 3) as usize, (mv[1] & 3) as usize);
        let sx = x as isize + (mv[0] >> 2) as isize;
        let sy = y as isize + (mv[1] >> 2) as isize;
        let (w, h) = (self.size.0 as isize, self.size.1 as isize);
        let pad = PAD as isize;
        // Every read is within one sample right and below of the block.
        if sx < -pad || sy < -pad || sx + 17 > w + pad || sy + 17 > h + pad {
            for r in 0..16 {
                for c in 0..16 {
                    out[r * 16 + c] = sample(rf, sx + c as isize, sy + r as isize, fx, fy);
                }
            }
            return;
        }
        let at = |dx: isize, dy: isize| {
            ((sy + dy + pad) as usize) * self.stride + (sx + dx + pad) as usize
        };
        // The two planes (and offsets) each fraction averages; one alone where both match.
        let (g, b, hh, j) = (&self.g, &self.b, &self.h, &self.j);
        let (p, q): (Read<'_>, Read<'_>) = match (fx, fy) {
            (0, 0) => ((g, at(0, 0)), (g, at(0, 0))),
            (0, 1) => ((g, at(0, 0)), (hh, at(0, 0))),
            (0, 2) => ((hh, at(0, 0)), (hh, at(0, 0))),
            (0, 3) => ((g, at(0, 1)), (hh, at(0, 0))),
            (1, 0) => ((g, at(0, 0)), (b, at(0, 0))),
            (1, 1) => ((b, at(0, 0)), (hh, at(0, 0))),
            (1, 2) => ((hh, at(0, 0)), (j, at(0, 0))),
            (1, 3) => ((hh, at(0, 0)), (b, at(0, 1))),
            (2, 0) => ((b, at(0, 0)), (b, at(0, 0))),
            (2, 1) => ((b, at(0, 0)), (j, at(0, 0))),
            (2, 2) => ((j, at(0, 0)), (j, at(0, 0))),
            (2, 3) => ((j, at(0, 0)), (b, at(0, 1))),
            (3, 0) => ((g, at(1, 0)), (b, at(0, 0))),
            (3, 1) => ((b, at(0, 0)), (hh, at(1, 0))),
            (3, 2) => ((j, at(0, 0)), (hh, at(1, 0))),
            _ => ((hh, at(1, 0)), (b, at(0, 1))),
        };
        for r in 0..16 {
            let a = &p.0[p.1 + r * self.stride..][..16];
            let c = &q.0[q.1 + r * self.stride..][..16];
            for ((o, &u), &v) in out[r * 16..][..16].iter_mut().zip(a).zip(c) {
                *o = ((u16::from(u) + u16::from(v) + 1) >> 1) as u8;
            }
        }
    }
}

/// One predicted luma sample at whole position (`x`, `y`) plus (`fx`, `fy`) quarters, from
/// the clamped reference as 8.4.2.2.1 writes it.
pub(super) fn sample(rf: &Plane, x: isize, y: isize, fx: usize, fy: usize) -> u8 {
    let px = |x: isize, y: isize| -> i32 {
        let x = x.clamp(0, rf.w as isize - 1) as usize;
        let y = y.clamp(0, rf.h as isize - 1) as usize;
        i32::from(rf.data[y * rf.w + x])
    };
    let b1 =
        |x: isize, y: isize| -> i32 { (0..6).map(|k| TAPS[k] * px(x - 2 + k as isize, y)).sum() };
    let h1 =
        |x: isize, y: isize| -> i32 { (0..6).map(|k| TAPS[k] * px(x, y - 2 + k as isize)).sum() };
    let half = |s: i32| i32::from(clip((s + 16) >> 5));
    let gs = |dx: isize, dy: isize| px(x + dx, y + dy);
    let b = |dy: isize| half(b1(x, y + dy));
    let h = |dx: isize| half(h1(x + dx, y));
    let j = || {
        let s: i32 = (0..6).map(|k| TAPS[k] * b1(x, y - 2 + k as isize)).sum();
        i32::from(clip((s + 512) >> 10))
    };
    let avg = |a: i32, b: i32| ((a + b + 1) >> 1) as u8;
    match (fx, fy) {
        (0, 0) => gs(0, 0) as u8,
        (0, 1) => avg(gs(0, 0), h(0)),
        (0, 2) => h(0) as u8,
        (0, 3) => avg(gs(0, 1), h(0)),
        (1, 0) => avg(gs(0, 0), b(0)),
        (1, 1) => avg(b(0), h(0)),
        (1, 2) => avg(h(0), j()),
        (1, 3) => avg(h(0), b(1)),
        (2, 0) => b(0) as u8,
        (2, 1) => avg(b(0), j()),
        (2, 2) => j() as u8,
        (2, 3) => avg(j(), b(1)),
        (3, 0) => avg(gs(1, 0), b(0)),
        (3, 1) => avg(b(0), h(1)),
        (3, 2) => avg(j(), h(1)),
        _ => avg(h(1), b(1)),
    }
}
