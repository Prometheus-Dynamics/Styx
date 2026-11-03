//! Styx's own H.264 encoder (pure Rust): Constrained Baseline, CAVLC, one slice and one
//! reference frame, the deblocking filter off. I frames are Intra16x16 (vertical,
//! horizontal or DC by SAD) with DC chroma; P frames add P_Skip and P_L0_16x16 with a
//! whole-pel diamond motion search. A fixed QP (no rate control yet).
//!
//! The decoder's reconstruction is kept bit for bit (no deblocking, so reference pictures are
//! exactly what a decoder holds), and a steady stream of one size allocates nothing.

mod bits;
mod cavlc;
mod codec;
#[cfg(all(feature = "gpu-h264", target_os = "linux"))]
mod gpu;
mod headers;
mod mb;
mod picture;
mod tables;
#[cfg(test)]
mod tests;
mod transform;

use bits::{Bits, nal};
pub use codec::StyxH264Encoder;
#[cfg(all(feature = "gpu-h264", target_os = "linux"))]
pub use gpu::{GPU_SEARCH_RANGE, GpuStyxH264};
use mb::{Counts, MbCode, write_residual};
use picture::{I16_DC, I16_HORIZONTAL, I16_VERTICAL, Picture};

use crate::CodecError;

/// Pictures to encode: 4:2:0 planes (chroma `width / 2` x `height / 2`) or grey (coded with
/// flat chroma). Width and height are even.
#[derive(Clone, Copy, Debug)]
pub enum StyxH264Input<'a> {
    /// Planar YUV 4:2:0.
    I420 {
        y: &'a [u8],
        u: &'a [u8],
        v: &'a [u8],
        width: usize,
        height: usize,
        y_stride: usize,
        c_stride: usize,
    },
    /// NV12: luma, then interleaved Cb Cr.
    Nv12 {
        y: &'a [u8],
        uv: &'a [u8],
        width: usize,
        height: usize,
        y_stride: usize,
        uv_stride: usize,
    },
    /// 8-bit grey.
    Gray {
        y: &'a [u8],
        width: usize,
        height: usize,
        stride: usize,
    },
}

impl StyxH264Input<'_> {
    pub fn size(&self) -> (usize, usize) {
        match *self {
            Self::I420 { width, height, .. }
            | Self::Nv12 { width, height, .. }
            | Self::Gray { width, height, .. } => (width, height),
        }
    }

    fn check(&self) -> Result<(), CodecError> {
        let fits = |plane: &[u8], stride: usize, row: usize, rows: usize| {
            stride >= row && plane.len() >= stride * (rows - 1) + row
        };
        let (w, h) = self.size();
        if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) || w > 8192 || h > 8192
        {
            return Err(CodecError::Codec(format!(
                "styx-h264: {w}x{h}: even sizes up to 8192"
            )));
        }
        let ok = match *self {
            Self::I420 {
                y,
                u,
                v,
                y_stride,
                c_stride,
                ..
            } => {
                fits(y, y_stride, w, h)
                    && fits(u, c_stride, w / 2, h / 2)
                    && fits(v, c_stride, w / 2, h / 2)
            }
            Self::Nv12 {
                y,
                uv,
                y_stride,
                uv_stride,
                ..
            } => fits(y, y_stride, w, h) && fits(uv, uv_stride, w, h / 2),
            Self::Gray { y, stride, .. } => fits(y, stride, w, h),
        };
        if ok {
            Ok(())
        } else {
            Err(CodecError::Codec("styx-h264: a plane is too short".into()))
        }
    }
}

/// The motion vector and reference of a coded macroblock (reference -1: intra).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Motion {
    mv: [i32; 2],
    ref_idx: i8,
}

/// The encoder: settings, the pictures (source copy, the one being coded, the reference)
/// and the per-picture state, all kept between pictures.
#[derive(Debug)]
pub struct StyxH264 {
    qp: u8,
    keyframe_interval: u32,
    size: (usize, usize),
    mbs: (usize, usize),
    src: Picture,
    cur: Picture,
    reference: Picture,
    motion: Vec<Motion>,
    /// Whole-pel search starts per macroblock from a motion search elsewhere (the GPU's),
    /// empty without one.
    hints: Vec<[i32; 2]>,
    counts: Counts,
    code: Box<MbCode>,
    bits: Bits,
    since_key: u32,
    frame_num: u32,
    idr_id: u32,
    force_key: bool,
}

/// The SAD-domain Lagrangian for motion vector bits at `qp` (√(0.85 · 2^((qp − 12) / 3))).
fn lambda(qp: u8) -> u32 {
    (0.85 * 2f64.powf((f64::from(qp) - 12.0) / 3.0))
        .sqrt()
        .round()
        .max(1.0) as u32
}

/// Bits of `se(v)`.
fn se_bits(v: i32) -> u32 {
    let k = if v > 0 { 2 * v - 1 } else { -2 * v } as u32;
    2 * (32 - (k + 1).leading_zeros()) - 1
}

fn median(a: i32, b: i32, c: i32) -> i32 {
    a + b + c - a.min(b).min(c) - a.max(b).max(c)
}

impl StyxH264 {
    /// An encoder at `qp` (0–51; 20–30 is typical) with an IDR picture every
    /// `keyframe_interval` pictures (at least 1).
    pub fn new(qp: u8, keyframe_interval: u32) -> Self {
        Self {
            qp: qp.min(51),
            keyframe_interval: keyframe_interval.max(1),
            size: (0, 0),
            mbs: (0, 0),
            src: Picture::default(),
            cur: Picture::default(),
            reference: Picture::default(),
            motion: Vec::new(),
            hints: Vec::new(),
            counts: Counts::default(),
            code: Box::default(),
            bits: Bits::default(),
            since_key: 0,
            frame_num: 0,
            idr_id: 0,
            force_key: true,
        }
    }

    pub fn qp(&self) -> u8 {
        self.qp
    }

    /// Makes the next picture an IDR picture.
    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Encodes `input` as Annex B NAL units into `out` (cleared first; its capacity reused):
    /// SPS and PPS before each IDR picture. Returns whether it was an IDR picture.
    pub fn encode(
        &mut self,
        input: &StyxH264Input<'_>,
        out: &mut Vec<u8>,
    ) -> Result<bool, CodecError> {
        self.encode_hinted(input, out, |_, _, _, hints| {
            hints.clear();
            Ok(())
        })
    }

    /// [`Self::encode`], with `hint` asked before a P picture for a whole-pel offset per
    /// macroblock (row-major) to start the search from: it gets the padded luma of the
    /// picture and of its reference (`w` x `h`, rows `w` apart) and fills the hints (left empty:
    /// none).
    pub fn encode_hinted(
        &mut self,
        input: &StyxH264Input<'_>,
        out: &mut Vec<u8>,
        hint: impl FnOnce(&[u8], &[u8], (usize, usize), &mut Vec<[i32; 2]>) -> Result<(), CodecError>,
    ) -> Result<bool, CodecError> {
        input.check()?;
        let size = input.size();
        if size != self.size {
            let mbs = (size.0.div_ceil(16), size.1.div_ceil(16));
            self.size = size;
            self.mbs = mbs;
            self.src = Picture::new(mbs);
            self.cur = Picture::new(mbs);
            self.reference = Picture::new(mbs);
            self.motion = vec![Motion::default(); mbs.0 * mbs.1];
            self.counts = Counts::new(mbs);
            self.force_key = true;
        }
        self.load(input);
        let idr = self.force_key || self.since_key >= self.keyframe_interval;
        self.hints.clear();
        if !idr {
            let (y, r) = (&self.src.y, &self.reference.y);
            hint(&y.data, &r.data, (y.w, y.h), &mut self.hints)?;
            if !self.hints.is_empty() && self.hints.len() != self.mbs.0 * self.mbs.1 {
                return Err(CodecError::Codec("styx-h264: a hint per macroblock".into()));
            }
        }
        out.clear();
        if idr {
            self.force_key = false;
            self.since_key = 0;
            self.frame_num = 0;
            self.bits.clear();
            headers::sps(&mut self.bits, size, self.mbs);
            nal(out, 3, 7, self.bits.bytes());
            self.bits.clear();
            headers::pps(&mut self.bits);
            nal(out, 3, 8, self.bits.bytes());
        }
        self.bits.clear();
        let idr_id = idr.then_some(self.idr_id);
        headers::slice_header(&mut self.bits, idr_id, self.frame_num, self.qp);
        self.code_picture(idr);
        self.bits.trailing();
        nal(
            out,
            if idr { 3 } else { 2 },
            if idr { 5 } else { 1 },
            self.bits.bytes(),
        );
        if idr {
            self.idr_id = (self.idr_id + 1) % 2;
        }
        self.since_key += 1;
        self.frame_num = (self.frame_num + 1) % (1 << headers::LOG2_MAX_FRAME_NUM);
        std::mem::swap(&mut self.cur, &mut self.reference);
        Ok(idr)
    }

    fn load(&mut self, input: &StyxH264Input<'_>) {
        let (w, h) = self.size;
        let half = (w / 2, h / 2);
        match *input {
            StyxH264Input::I420 {
                y,
                u,
                v,
                y_stride,
                c_stride,
                ..
            } => {
                self.src.y.fill(y, y_stride, 1, (w, h));
                self.src.u.fill(u, c_stride, 1, half);
                self.src.v.fill(v, c_stride, 1, half);
            }
            StyxH264Input::Nv12 {
                y,
                uv,
                y_stride,
                uv_stride,
                ..
            } => {
                self.src.y.fill(y, y_stride, 1, (w, h));
                self.src.u.fill(uv, uv_stride, 2, half);
                self.src.v.fill(&uv[1..], uv_stride, 2, half);
            }
            StyxH264Input::Gray { y, stride, .. } => {
                self.src.y.fill(y, stride, 1, (w, h));
                self.src.u.flat(128);
                self.src.v.flat(128);
            }
        }
    }

    fn code_picture(&mut self, idr: bool) {
        let (mb_w, mb_h) = self.mbs;
        let mut skip_run = 0u32;
        for mby in 0..mb_h {
            for mbx in 0..mb_w {
                if idr {
                    self.intra_mb((mbx, mby), None);
                } else {
                    self.p_mb((mbx, mby), &mut skip_run);
                }
            }
        }
        if skip_run > 0 {
            self.bits.ue(skip_run);
        }
    }

    fn chroma_qp(&self) -> u8 {
        tables::chroma_qp(self.qp)
    }

    /// The best Intra16x16 mode for the macroblock and its SAD.
    fn best_intra16(&self, at: (usize, usize), pred: &mut [u8; 256]) -> (u8, u32) {
        let mut best = (I16_DC, u32::MAX);
        let xy = (at.0 * 16, at.1 * 16);
        for mode in [I16_VERTICAL, I16_HORIZONTAL, I16_DC] {
            if picture::intra16(&self.cur.y, at, mode, pred) {
                let s = picture::sad16(&self.src.y, xy, pred);
                if s < best.1 {
                    best = (mode, s);
                }
            }
        }
        best
    }

    /// Codes the macroblock at `at` as Intra16x16 (`skip_run`: in a P slice).
    fn intra_mb(&mut self, at: (usize, usize), skip_run: Option<&mut u32>) {
        let mut pred = [0u8; 256];
        let (mode, _) = self.best_intra16(at, &mut pred);
        picture::intra16(&self.cur.y, at, mode, &mut pred);
        let (mut pu, mut pv) = ([0u8; 64], [0u8; 64]);
        picture::chroma_dc(&self.cur.u, at, &mut pu);
        picture::chroma_dc(&self.cur.v, at, &mut pv);
        let (xy, cxy) = ((at.0 * 16, at.1 * 16), (at.0 * 8, at.1 * 8));
        let (qp, qpc) = (self.qp, self.chroma_qp());
        let code = &mut self.code;
        code.code_luma(&self.src.y, xy, &pred, qp, true);
        code.code_chroma([&self.src.u, &self.src.v], cxy, [&pu, &pv], qpc, true);
        code.recon_luma(&mut self.cur.y, xy, &pred, qp, true);
        code.recon_chroma([&mut self.cur.u, &mut self.cur.v], cxy, [&pu, &pv], qpc);
        self.counts.set(at, Some(code), true);
        self.motion[at.1 * self.mbs.0 + at.0] = Motion {
            mv: [0, 0],
            ref_idx: -1,
        };
        let mb_type = 1
            + u32::from(mode)
            + 4 * u32::from(code.cbp_chroma)
            + if code.cbp_luma > 0 { 12 } else { 0 };
        if let Some(run) = skip_run {
            self.bits.ue(*run);
            *run = 0;
            self.bits.ue(5 + mb_type);
        } else {
            self.bits.ue(mb_type);
        }
        self.bits.ue(0); // intra_chroma_pred_mode: DC
        self.bits.se(0); // mb_qp_delta
        write_residual(&mut self.bits, code, &self.counts, at, true);
    }

    fn neighbour(&self, x: isize, y: isize) -> Option<Motion> {
        let (w, h) = (self.mbs.0 as isize, self.mbs.1 as isize);
        (x >= 0 && y >= 0 && x < w && y < h).then(|| self.motion[(y * w + x) as usize])
    }

    /// The 16x16 motion vector prediction (8.4.1.3) and the P_Skip vector (8.4.1.1).
    fn predict_mv(&self, (mbx, mby): (usize, usize)) -> ([i32; 2], [i32; 2]) {
        let (x, y) = (mbx as isize, mby as isize);
        let a = self.neighbour(x - 1, y);
        let b = self.neighbour(x, y - 1);
        let c = self
            .neighbour(x + 1, y - 1)
            .or_else(|| self.neighbour(x - 1, y - 1));
        let (mut pb, mut pc) = (b, c);
        if b.is_none() && c.is_none() && a.is_some() {
            (pb, pc) = (a, a);
        }
        let get = |n: Option<Motion>| {
            n.unwrap_or(Motion {
                mv: [0, 0],
                ref_idx: -1,
            })
        };
        let (ma, mb, mc) = (get(a), get(pb), get(pc));
        let refs = [ma.ref_idx == 0, mb.ref_idx == 0, mc.ref_idx == 0];
        let mvp = match refs {
            [true, false, false] => ma.mv,
            [false, true, false] => mb.mv,
            [false, false, true] => mc.mv,
            _ => [0, 1].map(|i| median(ma.mv[i], mb.mv[i], mc.mv[i])),
        };
        let zero = |n: Option<Motion>| n.is_some_and(|m| m.ref_idx == 0 && m.mv == [0, 0]);
        let skip = if a.is_none() || b.is_none() || zero(a) || zero(b) {
            [0, 0]
        } else {
            mvp
        };
        (mvp, skip)
    }

    /// Whole-pel diamond search around the best of zero, `mvp`, the hint and the neighbours'
    /// vectors, the block kept inside the reference; returns the vector (quarter pels) and its
    /// cost.
    fn search(&self, (mbx, mby): (usize, usize), mvp: [i32; 2], lambda: u32) -> ([i32; 2], u32) {
        let (x, y) = (mbx as i32 * 16, mby as i32 * 16);
        let (w, h) = (self.reference.y.w as i32, self.reference.y.h as i32);
        let cost = |d: [i32; 2]| -> Option<u32> {
            let (rx, ry) = (x + d[0], y + d[1]);
            if rx < 0
                || ry < 0
                || rx + 16 > w
                || ry + 16 > h
                || d[1].abs() > 128
                || d[0].abs() > 512
            {
                return None;
            }
            let sad = picture::sad16_at(
                &self.src.y,
                (x as usize, y as usize),
                &self.reference.y,
                (rx as usize, ry as usize),
            );
            Some(sad + lambda * (se_bits(4 * d[0] - mvp[0]) + se_bits(4 * d[1] - mvp[1])))
        };
        let start = [(mvp[0] + 2) >> 2, (mvp[1] + 2) >> 2];
        let mut best = ([0, 0], cost([0, 0]).unwrap_or(u32::MAX));
        let hint = self.hints.get(mby * self.mbs.0 + mbx).copied();
        // Neighbours' vectors: left, above and above-right from this picture; this
        // macroblock, the next and the one below as the last picture left them (not coded yet).
        let (bx, by) = (mbx as isize, mby as isize);
        let neighbours = [(-1, 0), (0, -1), (1, -1), (0, 0), (1, 0), (0, 1)].map(|(dx, dy)| {
            self.neighbour(bx + dx, by + dy)
                .filter(|m| m.ref_idx == 0)
                .map(|m| [m.mv[0] >> 2, m.mv[1] >> 2])
        });
        for cand in [Some(start), hint].into_iter().chain(neighbours).flatten() {
            if let Some(c) = cost(cand).filter(|&c| c < best.1) {
                best = (cand, c);
            }
        }
        for step in [16, 8, 4, 2, 1] {
            for _ in 0..16 {
                let centre = best.0;
                for d in [[step, 0], [-step, 0], [0, step], [0, -step]] {
                    let cand = [centre[0] + d[0], centre[1] + d[1]];
                    if let Some(c) = cost(cand).filter(|&c| c < best.1) {
                        best = (cand, c);
                    }
                }
                if best.0 == centre {
                    break;
                }
            }
        }
        ([best.0[0] * 4, best.0[1] * 4], best.1)
    }

    /// Codes `self.code` as an inter macroblock predicted at `mv`; leaves the predictions in
    /// `pred`.
    fn code_inter(
        &mut self,
        at: (usize, usize),
        mv: [i32; 2],
        pred: &mut ([u8; 256], [u8; 64], [u8; 64]),
    ) {
        let (xy, cxy) = ((at.0 * 16, at.1 * 16), (at.0 * 8, at.1 * 8));
        picture::luma_mc(&self.reference.y, xy, mv, &mut pred.0);
        picture::chroma_mc(&self.reference.u, cxy, mv, &mut pred.1);
        picture::chroma_mc(&self.reference.v, cxy, mv, &mut pred.2);
        let (qp, qpc) = (self.qp, self.chroma_qp());
        self.code.code_luma(&self.src.y, xy, &pred.0, qp, false);
        self.code.code_chroma(
            [&self.src.u, &self.src.v],
            cxy,
            [&pred.1, &pred.2],
            qpc,
            false,
        );
    }

    fn p_mb(&mut self, at: (usize, usize), skip_run: &mut u32) {
        let lambda = lambda(self.qp);
        let (mvp, skip_mv) = self.predict_mv(at);
        let mut pred = ([0u8; 256], [0u8; 64], [0u8; 64]);
        let (xy, cxy) = ((at.0 * 16, at.1 * 16), (at.0 * 8, at.1 * 8));
        let at_i = at.1 * self.mbs.0 + at.0;
        // P_Skip when the skip vector leaves nothing to code.
        self.code_inter(at, skip_mv, &mut pred);
        let skip = self.code.cbp_luma == 0 && self.code.cbp_chroma == 0;
        let mv = if skip {
            skip_mv
        } else {
            let (mv, cost) = self.search(at, mvp, lambda);
            let mut ipred = [0u8; 256];
            let (_, intra) = self.best_intra16(at, &mut ipred);
            if intra.saturating_add(8 * lambda) < cost {
                self.intra_mb(at, Some(skip_run));
                return;
            }
            self.code_inter(at, mv, &mut pred);
            mv
        };
        if skip || (mv == skip_mv && self.code.cbp_luma == 0 && self.code.cbp_chroma == 0) {
            self.cur.y.put(xy.0, xy.1, 16, &pred.0);
            self.cur.u.put(cxy.0, cxy.1, 8, &pred.1);
            self.cur.v.put(cxy.0, cxy.1, 8, &pred.2);
            self.counts.set(at, None, false);
            self.motion[at_i] = Motion {
                mv: skip_mv,
                ref_idx: 0,
            };
            *skip_run += 1;
            return;
        }
        let (qp, qpc) = (self.qp, self.chroma_qp());
        let code = &mut self.code;
        code.recon_luma(&mut self.cur.y, xy, &pred.0, qp, false);
        code.recon_chroma(
            [&mut self.cur.u, &mut self.cur.v],
            cxy,
            [&pred.1, &pred.2],
            qpc,
        );
        self.counts.set(at, Some(code), false);
        self.motion[at_i] = Motion { mv, ref_idx: 0 };
        self.bits.ue(*skip_run);
        *skip_run = 0;
        self.bits.ue(0); // mb_type: P_L0_16x16
        self.bits.se(mv[0] - mvp[0]);
        self.bits.se(mv[1] - mvp[1]);
        let cbp = usize::from(code.cbp_chroma << 4 | code.cbp_luma);
        self.bits.ue(u32::from(tables::INTER_CBP_CODE[cbp]));
        if cbp > 0 {
            self.bits.se(0); // mb_qp_delta
            write_residual(&mut self.bits, code, &self.counts, at, false);
        }
    }
}
