//! One macroblock's residual: transform and quantisation against a prediction, the
//! reconstruction the decoder will make of it, the coefficient counts CAVLC predicts from,
//! and the residual syntax (7.3.5.3).

use super::bits::Bits;
use super::cavlc;
use super::picture::Plane;
use super::tables::ZIGZAG;
use super::transform::{
    dequant_chroma_dc, dequant_luma_dc, quant_chroma_dc, quant_luma_dc, quantiser, scale,
};
use styx_core::simd::h264;

/// The (x, y) in 4x4 blocks of luma block `blk` (`luma4x4BlkIdx`, 8x8 quadrants in raster,
/// 4x4 blocks in raster within each).
pub(super) const fn blk_xy(blk: usize) -> (usize, usize) {
    let (i8, i4) = (blk / 4, blk % 4);
    ((i8 & 1) * 2 + (i4 & 1), (i8 >> 1) * 2 + (i4 >> 1))
}

/// A macroblock's levels, blocks in raster order over the macroblock, raster within each.
#[derive(Debug, Default)]
pub(super) struct MbCode {
    /// Luma blocks (`by * 4 + bx`); Intra16x16 keeps position 0 at zero.
    pub(super) luma: [[i32; 16]; 16],
    /// Intra16x16 luma DC, raster by block position.
    pub(super) luma_dc: [i32; 16],
    /// Chroma DC (Cb, Cr), raster by block position.
    pub(super) chroma_dc: [[i32; 4]; 2],
    /// Chroma blocks (Cb, Cr) in raster; position 0 at zero.
    pub(super) chroma_ac: [[[i32; 16]; 4]; 2],
    /// `CodedBlockPatternLuma`: a bit per 8x8 quadrant (Intra16x16: 0 or 15).
    pub(super) cbp_luma: u8,
    /// `CodedBlockPatternChroma`: 0, 1 (DC only) or 2.
    pub(super) cbp_chroma: u8,
}

/// The 8x8 quadrant of raster luma block `b`.
const fn quadrant(b: usize) -> usize {
    (b / 8) * 2 + (b % 4) / 2
}

/// `plane` from (`x`, `y`) on, for a kernel's grid.
fn from(plane: &Plane, (x, y): (usize, usize)) -> &[u8] {
    &plane.data[y * plane.w + x..]
}

fn from_mut(plane: &mut Plane, (x, y): (usize, usize)) -> &mut [u8] {
    &mut plane.data[y * plane.w + x..]
}

impl MbCode {
    /// Quantises luma: `src`'s macroblock at `xy` against `pred` (16x16), Intra16x16 (DC
    /// through its own transform) or inter.
    pub(super) fn code_luma(
        &mut self,
        src: &Plane,
        xy: (usize, usize),
        pred: &[u8; 256],
        qp: u8,
        i16: bool,
    ) {
        let mut dc = [0i32; 16];
        let q = quantiser(qp, i16);
        h264::residual_quant(
            from(src, xy),
            src.w,
            pred,
            16,
            (4, 4),
            &q,
            i16,
            &mut dc,
            &mut self.luma,
        );
        let mut nz8 = [false; 4];
        for (b, l) in self.luma.iter().enumerate() {
            nz8[quadrant(b)] |= l.iter().any(|&v| v != 0);
        }
        if i16 {
            quant_luma_dc(&dc, qp, &mut self.luma_dc);
            self.cbp_luma = if nz8.iter().any(|&n| n) { 15 } else { 0 };
        } else {
            self.luma_dc = [0; 16];
            self.cbp_luma = nz8.iter().enumerate().map(|(i, &n)| u8::from(n) << i).sum();
            for (b, l) in self.luma.iter_mut().enumerate() {
                if self.cbp_luma >> quadrant(b) & 1 == 0 {
                    *l = [0; 16];
                }
            }
        }
    }

    /// Quantises chroma: `src`'s 8x8 blocks at `xy` against `pred` (Cb, Cr).
    pub(super) fn code_chroma(
        &mut self,
        src: [&Plane; 2],
        xy: (usize, usize),
        pred: [&[u8; 64]; 2],
        qpc: u8,
        intra: bool,
    ) {
        let (mut any_dc, mut any_ac) = (false, false);
        let q = quantiser(qpc, intra);
        for c in 0..2 {
            let mut dc = [0i32; 4];
            let ac = &mut self.chroma_ac[c];
            h264::residual_quant(
                from(src[c], xy),
                src[c].w,
                pred[c],
                8,
                (2, 2),
                &q,
                true,
                &mut dc,
                ac,
            );
            any_ac |= ac.iter().flatten().any(|&v| v != 0);
            any_dc |= quant_chroma_dc(&dc, qpc, intra, &mut self.chroma_dc[c]) > 0;
        }
        self.cbp_chroma = if any_ac { 2 } else { u8::from(any_dc) };
        if !any_ac {
            self.chroma_ac = [[[0; 16]; 4]; 2];
        }
    }

    /// The decoder's luma: `pred` plus the dequantised residual, into `rec` at `xy`.
    pub(super) fn recon_luma(
        &self,
        rec: &mut Plane,
        xy: (usize, usize),
        pred: &[u8; 256],
        qp: u8,
        i16: bool,
    ) {
        let dc = i16.then(|| dequant_luma_dc(&self.luma_dc, qp));
        let w = rec.w;
        h264::recon(
            &self.luma,
            &scale(qp),
            dc.as_ref().map(|d| &d[..]),
            pred,
            16,
            from_mut(rec, xy),
            w,
            (4, 4),
        );
    }

    /// The decoder's chroma into `rec` (Cb, Cr) at `xy`.
    pub(super) fn recon_chroma(
        &self,
        rec: [&mut Plane; 2],
        xy: (usize, usize),
        pred: [&[u8; 64]; 2],
        qpc: u8,
    ) {
        let sc = scale(qpc);
        for (c, plane) in rec.into_iter().enumerate() {
            let dc = dequant_chroma_dc(&self.chroma_dc[c], qpc);
            let w = plane.w;
            h264::recon(
                &self.chroma_ac[c],
                &sc,
                Some(&dc),
                pred[c],
                8,
                from_mut(plane, xy),
                w,
                (2, 2),
            );
        }
    }
}

/// Total coefficient counts per 4x4 block over the picture, for CAVLC's nC.
#[derive(Debug, Default)]
pub(super) struct Counts {
    /// Luma, `mb_w * 4` per row.
    pub(super) luma: Vec<u8>,
    /// Cb and Cr, `mb_w * 2` per row each.
    pub(super) chroma: [Vec<u8>; 2],
    /// Width in macroblocks.
    pub(super) mb_w: usize,
}

impl Counts {
    pub(super) fn new((mb_w, mb_h): (usize, usize)) -> Self {
        Self {
            luma: vec![0; mb_w * mb_h * 16],
            chroma: [vec![0; mb_w * mb_h * 4], vec![0; mb_w * mb_h * 4]],
            mb_w,
        }
    }

    /// nC for the luma block at (`x`, `y`) in 4x4 blocks.
    fn luma_nc(&self, x: usize, y: usize) -> i32 {
        let w = self.mb_w * 4;
        cavlc::predict_nc(
            (x > 0).then(|| self.luma[y * w + x - 1]),
            (y > 0).then(|| self.luma[(y - 1) * w + x]),
        )
    }

    fn chroma_nc(&self, c: usize, x: usize, y: usize) -> i32 {
        let w = self.mb_w * 2;
        cavlc::predict_nc(
            (x > 0).then(|| self.chroma[c][y * w + x - 1]),
            (y > 0).then(|| self.chroma[c][(y - 1) * w + x]),
        )
    }

    /// Records macroblock (`mbx`, `mby`)'s counts from `code` (all zero for `None`, a skip).
    pub(super) fn set(&mut self, (mbx, mby): (usize, usize), code: Option<&MbCode>, i16: bool) {
        let nz = |b: &[i32; 16], from: usize| b[from..].iter().filter(|&&v| v != 0).count() as u8;
        let w = self.mb_w * 4;
        for blk in 0..16 {
            let (bx, by) = blk_xy(blk);
            self.luma[(mby * 4 + by) * w + mbx * 4 + bx] = match code {
                Some(c) if c.cbp_luma >> (blk / 4) & 1 != 0 => {
                    nz(&c.luma[by * 4 + bx], usize::from(i16))
                }
                _ => 0,
            };
        }
        let w = self.mb_w * 2;
        for c in 0..2 {
            for b in 0..4 {
                self.chroma[c][(mby * 2 + (b >> 1)) * w + mbx * 2 + (b & 1)] = match code {
                    Some(m) if m.cbp_chroma == 2 => nz(&m.chroma_ac[c][b], 1),
                    _ => 0,
                };
            }
        }
    }
}

/// Writes macroblock (`mbx`, `mby`)'s `residual()` (its counts already [`Counts::set`]).
pub(super) fn write_residual(
    bits: &mut Bits,
    code: &MbCode,
    counts: &Counts,
    (mbx, mby): (usize, usize),
    i16: bool,
) {
    let scan = |b: &[i32; 16], from: usize, out: &mut [i32; 16]| {
        for (k, o) in out.iter_mut().enumerate().take(16 - from) {
            *o = b[usize::from(ZIGZAG[k + from])];
        }
        16 - from
    };
    let mut buf = [0i32; 16];
    if i16 {
        let n = scan(&code.luma_dc, 0, &mut buf);
        cavlc::block(bits, &buf[..n], counts.luma_nc(mbx * 4, mby * 4));
    }
    for blk in 0..16 {
        if code.cbp_luma >> (blk / 4) & 1 == 0 {
            continue;
        }
        let (bx, by) = blk_xy(blk);
        let n = scan(&code.luma[by * 4 + bx], usize::from(i16), &mut buf);
        cavlc::block(bits, &buf[..n], counts.luma_nc(mbx * 4 + bx, mby * 4 + by));
    }
    if code.cbp_chroma > 0 {
        for dc in &code.chroma_dc {
            cavlc::block(bits, dc, -1);
        }
    }
    if code.cbp_chroma == 2 {
        for c in 0..2 {
            for b in 0..4 {
                let n = scan(&code.chroma_ac[c][b], 1, &mut buf);
                let nc = counts.chroma_nc(c, mbx * 2 + (b & 1), mby * 2 + (b >> 1));
                cavlc::block(bits, &buf[..n], nc);
            }
        }
    }
}
