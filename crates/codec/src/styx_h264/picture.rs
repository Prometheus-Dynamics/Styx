//! Pictures in macroblock-padded planes, and what is read from them: the input copied in
//! with its edges repeated, intra prediction (Intra16x16, chroma DC), motion-compensated
//! prediction (whole-pel luma, bilinear chroma, coordinates clamped as the decoder clamps
//! them) and SAD.

/// One plane, `w` x `h`, rows `w` apart.
#[derive(Debug, Default)]
pub(super) struct Plane {
    pub(super) data: Vec<u8>,
    pub(super) w: usize,
    pub(super) h: usize,
}

impl Plane {
    fn new(w: usize, h: usize) -> Self {
        Self {
            data: vec![0; w * h],
            w,
            h,
        }
    }

    pub(super) fn at(&self, x: usize, y: usize) -> u8 {
        self.data[y * self.w + x]
    }

    /// The sample at (`x`, `y`) with both clamped into the plane (8.4.2.2).
    fn clamped(&self, x: isize, y: isize) -> u8 {
        let x = x.clamp(0, self.w as isize - 1) as usize;
        let y = y.clamp(0, self.h as isize - 1) as usize;
        self.data[y * self.w + x]
    }

    /// Copies `w` x `h` samples of `src` (rows `stride` apart, `step` bytes per sample) in,
    /// repeating the last column and row over the padding.
    pub(super) fn fill(&mut self, src: &[u8], stride: usize, step: usize, (w, h): (usize, usize)) {
        for y in 0..self.h {
            let row = &src[y.min(h - 1) * stride..];
            let dst = &mut self.data[y * self.w..][..self.w];
            if step == 1 {
                dst[..w].copy_from_slice(&row[..w]);
            } else {
                for (d, s) in dst[..w].iter_mut().zip(row.iter().step_by(step)) {
                    *d = *s;
                }
            }
            let last = dst[w - 1];
            dst[w..].fill(last);
        }
    }

    /// Fills the plane with `v`.
    pub(super) fn flat(&mut self, v: u8) {
        self.data.fill(v);
    }

    /// Writes an `n` x `n` block at (`x`, `y`).
    pub(super) fn put(&mut self, x: usize, y: usize, n: usize, block: &[u8]) {
        for r in 0..n {
            self.data[(y + r) * self.w + x..][..n].copy_from_slice(&block[r * n..][..n]);
        }
    }
}

/// Luma and the two chroma planes, 4:2:0.
#[derive(Debug, Default)]
pub(super) struct Picture {
    pub(super) y: Plane,
    pub(super) u: Plane,
    pub(super) v: Plane,
}

impl Picture {
    /// A picture of `mb_w` x `mb_h` macroblocks.
    pub(super) fn new((mb_w, mb_h): (usize, usize)) -> Self {
        Self {
            y: Plane::new(mb_w * 16, mb_h * 16),
            u: Plane::new(mb_w * 8, mb_h * 8),
            v: Plane::new(mb_w * 8, mb_h * 8),
        }
    }
}

/// Intra16x16 prediction modes.
pub(super) const I16_VERTICAL: u8 = 0;
pub(super) const I16_HORIZONTAL: u8 = 1;
pub(super) const I16_DC: u8 = 2;

/// Intra16x16 prediction of macroblock (`mbx`, `mby`) from `rec` in `mode`, or `None` when
/// the mode needs a neighbour that is not there.
pub(super) fn intra16(
    rec: &Plane,
    (mbx, mby): (usize, usize),
    mode: u8,
    out: &mut [u8; 256],
) -> bool {
    let (x0, y0) = (mbx * 16, mby * 16);
    let (top, left) = (mby > 0, mbx > 0);
    match mode {
        I16_VERTICAL if top => {
            for r in 0..16 {
                out[r * 16..][..16].copy_from_slice(&rec.data[(y0 - 1) * rec.w + x0..][..16]);
            }
        }
        I16_HORIZONTAL if left => {
            for r in 0..16 {
                out[r * 16..][..16].fill(rec.at(x0 - 1, y0 + r));
            }
        }
        I16_DC => {
            let st: u32 = if top {
                (0..16).map(|i| u32::from(rec.at(x0 + i, y0 - 1))).sum()
            } else {
                0
            };
            let sl: u32 = if left {
                (0..16).map(|i| u32::from(rec.at(x0 - 1, y0 + i))).sum()
            } else {
                0
            };
            let dc = match (top, left) {
                (true, true) => (st + sl + 16) >> 5,
                (true, false) => (st + 8) >> 4,
                (false, true) => (sl + 8) >> 4,
                (false, false) => 128,
            };
            out.fill(dc as u8);
        }
        _ => return false,
    }
    true
}

/// Chroma DC prediction (8.3.4.1-3) of macroblock (`mbx`, `mby`): each 4x4 block from the
/// four samples above and/or left of it, by the block's position.
pub(super) fn chroma_dc(rec: &Plane, (mbx, mby): (usize, usize), out: &mut [u8; 64]) {
    let (x0, y0) = (mbx * 8, mby * 8);
    let (top, left) = (mby > 0, mbx > 0);
    for b in 0..4 {
        let (bx, by) = ((b & 1) * 4, (b >> 1) * 4);
        let st = || -> u32 { (0..4).map(|i| u32::from(rec.at(x0 + bx + i, y0 - 1))).sum() };
        let sl = || -> u32 { (0..4).map(|i| u32::from(rec.at(x0 - 1, y0 + by + i))).sum() };
        let dc = match (bx, by) {
            // Top-right: the samples above first.
            (4, 0) if top => (st() + 2) >> 2,
            (4, 0) if left => (sl() + 2) >> 2,
            // Bottom-left: the samples to the left first.
            (0, 4) if left => (sl() + 2) >> 2,
            (0, 4) if top => (st() + 2) >> 2,
            (4, 0) | (0, 4) => 128,
            _ => match (top, left) {
                (true, true) => (st() + sl() + 4) >> 3,
                (true, false) => (st() + 2) >> 2,
                (false, true) => (sl() + 2) >> 2,
                (false, false) => 128,
            },
        };
        for r in 0..4 {
            out[(by + r) * 8 + bx..][..4].fill(dc as u8);
        }
    }
}

/// Luma prediction of the 16x16 block at (`x`, `y`) moved by the whole-pel `mv` (quarter
/// pels, a multiple of 4).
pub(super) fn luma_mc(rf: &Plane, (x, y): (usize, usize), mv: [i32; 2], out: &mut [u8; 256]) {
    debug_assert!(mv[0] % 4 == 0 && mv[1] % 4 == 0);
    let (sx, sy) = (
        x as isize + (mv[0] >> 2) as isize,
        y as isize + (mv[1] >> 2) as isize,
    );
    let inside = sx >= 0 && sy >= 0 && sx as usize + 16 <= rf.w && sy as usize + 16 <= rf.h;
    for r in 0..16 {
        let o = &mut out[r * 16..][..16];
        if inside {
            o.copy_from_slice(&rf.data[(sy as usize + r) * rf.w + sx as usize..][..16]);
        } else {
            for (c, v) in o.iter_mut().enumerate() {
                *v = rf.clamped(sx + c as isize, sy + r as isize);
            }
        }
    }
}

/// Chroma prediction of the 8x8 block at (`x`, `y`) moved by `mv` (eighth chroma pels, the
/// luma vector as it is): bilinear (8.4.2.2.2).
pub(super) fn chroma_mc(rf: &Plane, (x, y): (usize, usize), mv: [i32; 2], out: &mut [u8; 64]) {
    let (fx, fy) = (mv[0] & 7, mv[1] & 7);
    let (sx, sy) = (
        x as isize + (mv[0] >> 3) as isize,
        y as isize + (mv[1] >> 3) as isize,
    );
    for r in 0..8 {
        for c in 0..8 {
            let (px, py) = (sx + c as isize, sy + r as isize);
            let a = i32::from(rf.clamped(px, py));
            out[r * 8 + c] = if fx == 0 && fy == 0 {
                a as u8
            } else {
                let b = i32::from(rf.clamped(px + 1, py));
                let cc = i32::from(rf.clamped(px, py + 1));
                let d = i32::from(rf.clamped(px + 1, py + 1));
                (((8 - fx) * (8 - fy) * a
                    + fx * (8 - fy) * b
                    + (8 - fx) * fy * cc
                    + fx * fy * d
                    + 32)
                    >> 6) as u8
            };
        }
    }
}

/// SAD of the 16x16 block at (`x`, `y`) of `src` against `pred`.
pub(super) fn sad16(src: &Plane, (x, y): (usize, usize), pred: &[u8; 256]) -> u32 {
    styx_core::simd::h264::sad16(&src.data[y * src.w + x..], src.w, pred, 16)
}

/// SAD of the 16x16 block at (`x`, `y`) of `src` against `rf` at (`rx`, `ry`), both inside.
pub(super) fn sad16_at(
    src: &Plane,
    (x, y): (usize, usize),
    rf: &Plane,
    (rx, ry): (usize, usize),
) -> u32 {
    styx_core::simd::h264::sad16(
        &src.data[y * src.w + x..],
        src.w,
        &rf.data[ry * rf.w + rx..],
        rf.w,
    )
}
