//! The row loop: a worker turns a band of output rows into pixels, keeping a small ring of
//! front-end rows (unpacked, black level, gains, lens shading) around the current row.

use alloc::{vec, vec::Vec};

use crate::format::RawPacking;
use crate::output::{OutputBuffers, Scale};
use crate::params::Demosaic;
use crate::prepare::{Arith, IntPrep, Prepared};
use crate::simd::half::{self, Chroma, ColourOut};
use crate::simd::{self, RowKind};
use crate::stats::StatsAccum;

/// Samples of padding on each side of a front row (reflected neighbours for the 5x5 filters).
const PAD: usize = 2;
/// Front rows kept: more than the 5-row window, so a window never evicts itself.
const SLOTS: usize = 8;

/// The raw input frame.
pub(crate) struct Source<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub packing: RawPacking,
    /// Copy rows into the staging buffer before unpacking.
    pub copy: bool,
}

/// Which part of the frame a call makes, and how: output pixel `(0, 0)` and the mosaic around
/// it. Origins are even (so the Bayer phase is the frame's) and the front end's columns start on
/// a multiple of 4 (a whole packed group of every packing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geom {
    pub sample: Sample,
    /// The mosaic column and row of output pixel `(0, 0)` (its quad's, when sampling quads).
    pub x0: usize,
    pub y0: usize,
    /// Output pixels per row.
    pub ow: usize,
    /// Front rows hold mosaic columns `fx0 .. fx0 + fw`: the output's, with the neighbours the
    /// demosaic reads (reflected only at the frame's edges).
    pub fx0: usize,
    pub fw: usize,
    /// Gather the whole frame's statistics (geometries of the frame's full width only).
    pub stats: bool,
}

/// How output pixels come from the mosaic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sample {
    /// One per mosaic pixel, demosaiced ([`Scale::Full`]).
    Full,
    /// One per quad (2x2 cell): every `step`-th quad of every `step`-th quad row (1:
    /// [`Scale::Half`], each quad; more: a binned picture of 1/(2 `step`) size).
    Quads { step: usize },
}

impl Geom {
    /// The whole frame at `scale`.
    pub fn frame(width: usize, scale: Scale, stats: bool) -> Self {
        let sample = match scale {
            Scale::Full => Sample::Full,
            Scale::Half => Sample::Quads { step: 1 },
        };
        let ow = match scale {
            Scale::Full => width,
            Scale::Half => width / 2,
        };
        Self {
            sample,
            x0: 0,
            y0: 0,
            ow,
            fx0: 0,
            fw: width,
            stats,
        }
    }

    /// Mosaic columns `x0 .. x0 + w` and the rows from `y0` (both even) at `scale`, of a
    /// `width`-wide frame; `margin`: neighbours the demosaic reads on each side.
    pub fn window(
        width: usize,
        (x0, y0, w): (usize, usize, usize),
        scale: Scale,
        margin: usize,
    ) -> Self {
        let (sample, ow, margin) = match scale {
            Scale::Full => (Sample::Full, w, margin),
            Scale::Half => (Sample::Quads { step: 1 }, w / 2, 0),
        };
        let fx0 = x0.saturating_sub(margin) & !3;
        let fx1 = (x0 + w + margin).next_multiple_of(4).min(width);
        Self {
            sample,
            x0,
            y0,
            ow,
            fx0,
            fw: fx1 - fx0,
            stats: false,
        }
    }

    /// The whole frame, every `step`-th quad of every `step`-th quad row: `out_w` pixels per
    /// row.
    pub fn quads(width: usize, step: usize, out_w: usize, stats: bool) -> Self {
        Self {
            sample: Sample::Quads { step },
            x0: 0,
            y0: 0,
            ow: out_w,
            fx0: 0,
            fw: width,
            stats,
        }
    }

    /// Index of output column 0's sample in a front row (`PAD` + its offset in the span).
    fn col(&self) -> usize {
        PAD + self.x0 - self.fx0
    }
}

/// Bytes of input copied per block into the staging buffer.
const STAGE_BYTES: usize = 16 * 1024;

/// A cached copy of a few consecutive input rows. Frames often sit in uncached (write-combined)
/// DMA memory, where the unpack kernels' small overlapping loads are several times slower
/// than one large copy.
#[derive(Default)]
pub(crate) struct Stage {
    buf: Vec<u8>,
    first: usize,
    rows: usize,
    /// The bytes of each row held (offset in the row, length) and their pitch in `buf`.
    span: (usize, usize),
    pitch: usize,
}

impl Stage {
    /// Forget the rows held.
    pub fn clear(&mut self) {
        self.rows = 0;
    }
}

impl Source<'_> {
    /// Bytes `off .. off + bytes` of input row `y`, from the staging copy (refilled from `y`
    /// on a miss): whole rows in one copy, parts of rows (a window) row by row.
    fn row<'s>(
        &'s self,
        stage: &'s mut Stage,
        y: usize,
        height: usize,
        (off, bytes): (usize, usize),
    ) -> &'s [u8] {
        if !self.copy {
            return &self.data[y * self.stride + off..][..bytes];
        }
        if stage.span != (off, bytes) || !(stage.first..stage.first + stage.rows).contains(&y) {
            let whole = off == 0;
            let pitch = if whole { self.stride } else { bytes };
            let rows = (STAGE_BYTES / pitch).clamp(1, height - y);
            let len = pitch * (rows - 1) + bytes;
            stage.buf.resize(len.max(stage.buf.len()), 0);
            if whole {
                stage.buf[..len].copy_from_slice(&self.data[y * self.stride..][..len]);
            } else {
                for (r, dst) in stage.buf[..len].chunks_mut(pitch).enumerate() {
                    dst.copy_from_slice(&self.data[(y + r) * self.stride + off..][..dst.len()]);
                }
            }
            stage.first = y;
            stage.rows = rows;
            stage.span = (off, bytes);
            stage.pitch = pitch;
        }
        &stage.buf[(y - stage.first) * stage.pitch..][..bytes]
    }

    /// The bytes of mosaic columns `x0 .. x0 + width` (`x0` a multiple of 4) in a row.
    fn span(&self, x0: usize, width: usize) -> (usize, usize) {
        (self.packing.row_bytes(x0), self.packing.row_bytes(width))
    }

    fn unpack(&self, stage: &mut Stage, y: usize, height: usize, g: &Geom, dst: &mut [u16]) {
        let width = g.fw;
        let row = self.row(stage, y, height, self.span(g.fx0, width));
        match self.packing {
            RawPacking::Csi2Raw10 => drop(simd::unpack_raw10_row(row, dst, width)),
            RawPacking::Csi2Raw12 => drop(simd::unpack_raw12_row(row, dst, width)),
            RawPacking::U8 => dst[..width]
                .iter_mut()
                .zip(row)
                .for_each(|(d, &s)| *d = s as u16),
            RawPacking::U16Le { bits } => {
                let max = ((1u32 << bits) - 1) as u16;
                for (d, s) in dst[..width].iter_mut().zip(row.chunks_exact(2)) {
                    *d = u16::from_le_bytes([s[0], s[1]]).min(max);
                }
            }
        }
    }
}

/// Mirror row or column `i` into `0..n`, keeping its parity (so its colour).
fn reflect(i: isize, n: usize) -> usize {
    let n = n as isize;
    let r = if i < 0 {
        -i
    } else if i >= n {
        2 * n - 2 - i
    } else {
        i
    };
    r.clamp(0, n - 1) as usize
}

/// `row` from column `x0`.
fn from_col(row: half::LscRow<'_>, x0: usize) -> half::LscRow<'_> {
    half::LscRow {
        a: &row.a[x0..],
        d: &row.d[x0..],
        t: row.t,
    }
}

/// Where [`Worker::colour_row`] puts a row besides `rgb8[k]`.
enum Dest<'a> {
    Packed(&'a mut [u8]),
    Luma(&'a mut [u8]),
    /// Luma, Cb/Cr (interleaved, or Cb with Cr apart).
    LumaChroma(&'a mut [u8], &'a mut [u8], Option<&'a mut [u8]>),
}

/// Scratch rows of one band's processing.
#[derive(Default)]
pub(crate) struct Worker {
    width: usize,
    slots: Vec<Vec<u16>>,
    tags: [Option<usize>; SLOTS],
    gains: Vec<u16>,
    rgb16: [Vec<u16>; 3],
    rgb8: [[Vec<u8>; 3]; 2],
    luma16: Vec<u16>,
    quad: [Vec<u16>; 3],
    /// Sampled quads of two front rows (every `step`-th quad).
    dec: [Vec<u16>; 2],
    stage: Stage,
    pub stats: Option<StatsAccum>,
}

impl Worker {
    /// Size the scratch for `p` and reset the statistics, at the start of a frame.
    pub fn begin_frame(&mut self, p: &Prepared, stats: bool) {
        let w = p.width;
        if self.width != w {
            self.width = w;
            self.slots = vec![vec![0; w + 2 * PAD]; SLOTS];
            self.rgb16 = core::array::from_fn(|_| vec![0; w]);
            self.rgb8 = core::array::from_fn(|_| core::array::from_fn(|_| vec![0; w]));
            self.luma16 = vec![0; w];
            self.quad = core::array::from_fn(|_| vec![0; w / 2]);
            self.dec = core::array::from_fn(|_| vec![0; w]);
        }
        match (p.stats.as_ref().filter(|_| stats), &mut self.stats) {
            (Some(setup), Some(acc))
                if acc.histogram.len() == 4 * setup.config.histogram_bins as usize =>
            {
                acc.reset()
            }
            (Some(setup), acc) => *acc = Some(StatsAccum::new(setup)),
            (None, acc) => *acc = None,
        }
    }

    /// The slot holding front row `y` (reflected into the frame), columns `g.fx0 ..`,
    /// computing it if needed.
    fn front(&mut self, p: &Prepared, src: &Source, g: &Geom, y: isize) -> usize {
        let ry = reflect(y, p.height);
        let slot = ry % SLOTS;
        if self.tags[slot] == Some(ry) {
            return slot;
        }
        let (x0, w) = (g.fx0, g.fw);
        let row = &mut self.slots[slot];
        if let (Arith::Half(h), RawPacking::Csi2Raw10) = (&p.arith, src.packing) {
            let bytes = src.row(&mut self.stage, ry, p.height, src.span(x0, w));
            let lsc = h.lsc.as_ref().map(|l| from_col(l.row(ry), x0));
            let (black, gain) = (h.black[ry & 1], h.gain[ry & 1]);
            half::front_raw10_row(bytes, &mut row[PAD..PAD + w], black, gain, lsc, w);
        } else {
            src.unpack(&mut self.stage, ry, p.height, g, &mut row[PAD..PAD + w]);
        }
        match &p.arith {
            Arith::Int(ip) => {
                let gains = ip.gains.row(ry, x0, w, &mut self.gains);
                simd::front_row(&mut row[PAD..], ip.black[ry & 1], gains, ip.shift, w);
            }
            Arith::Half(_) if src.packing == RawPacking::Csi2Raw10 => {}
            Arith::Half(h) => {
                let lsc = h.lsc.as_ref().map(|l| from_col(l.row(ry), x0));
                half::front_row(&mut row[PAD..], h.black[ry & 1], h.gain[ry & 1], lsc, w);
            }
        }
        for k in 1..=PAD {
            row[PAD - k] = row[PAD + k];
            row[PAD + w - 1 + k] = row[PAD + w - 1 - k];
        }
        self.tags[slot] = Some(ry);
        slot
    }

    /// Front rows `y - r ..= y + r`.
    fn window<const N: usize>(
        &mut self,
        p: &Prepared,
        src: &Source,
        g: &Geom,
        y: usize,
    ) -> [usize; N] {
        let r = (N / 2) as isize;
        core::array::from_fn(|i| self.front(p, src, g, y as isize - r + i as isize))
    }

    /// The mosaic row of output row `oy`'s pixels (the top row of its quads).
    fn mosaic_row(g: &Geom, oy: usize) -> usize {
        match g.sample {
            Sample::Full => g.y0 + oy,
            Sample::Quads { step } => g.y0 + 2 * step * oy,
        }
    }

    /// The two front rows of output row `oy`'s quads: slots, or with `step` > 1 the sampled
    /// quads gathered into `dec`.
    fn quad_rows(&mut self, p: &Prepared, src: &Source, g: &Geom, oy: usize) -> Quads {
        let y = Self::mosaic_row(g, oy) as isize;
        let (t, b) = (self.front(p, src, g, y), self.front(p, src, g, y + 1));
        match g.sample {
            Sample::Quads { step } if step > 1 => {
                let col = g.col();
                for (dst, s) in self.dec.iter_mut().zip([t, b]) {
                    let row = &self.slots[s][col..];
                    let quads = row.chunks(2 * step);
                    for (d, q) in dst[..2 * g.ow].chunks_exact_mut(2).zip(quads) {
                        d.copy_from_slice(&q[..2]);
                    }
                }
                Quads::Gathered
            }
            _ => Quads::Slots(t, b),
        }
    }

    /// Output row `oy`: planar 8-bit RGB in `rgb8[k]` and its luma, or packed instead, or (fp16 only, the second row of a 4:2:0 pair, `k` 1) luma and the
    /// pair's chroma with `rgb8[0]` holding the first row.
    fn colour_row(
        &mut self,
        p: &Prepared,
        src: &Source,
        g: &Geom,
        oy: usize,
        k: usize,
        dest: Dest,
    ) {
        let n = g.ow;
        let my = Self::mosaic_row(g, oy);
        let col = g.col();
        if let Arith::Half(h) = &p.arith {
            let rows = match g.sample {
                Sample::Full => Ok(self.window::<3>(p, src, g, my)),
                Sample::Quads { .. } => Err(self.quad_rows(p, src, g, oy)),
            };
            let [first, second] = &mut self.rgb8;
            let out = match dest {
                Dest::LumaChroma(y, u, v) => {
                    let top = [&first[0][..], &first[1][..], &first[2][..]];
                    let [r, g, b] = second;
                    ColourOut::LumaChroma([r, g, b], y, Chroma { top, u, v }, &p.yuv)
                }
                dest => {
                    let [r, g, b] = if k == 0 { first } else { second };
                    match dest {
                        Dest::Packed(d) => ColourOut::Packed(d),
                        Dest::Luma(y) => ColourOut::PlanesLuma([r, g, b], y, &p.yuv),
                        Dest::LumaChroma(..) => unreachable!(),
                    }
                }
            };
            match rows {
                Ok(slots) => {
                    let rows = slots.map(|s| &self.slots[s][col - 1..]);
                    half::colour_row(rows, out, n, &h.colour[my & 1], &h.tone);
                }
                Err(q) => {
                    let (t, b) = q.rows(&self.slots, &self.dec, col);
                    half::quad_colour_row(t, b, out, n, &h.quad, p.pattern, &h.tone);
                }
            }
            return;
        }
        let Arith::Int(ip) = &p.arith else {
            unreachable!()
        };
        match g.sample {
            Sample::Full => {
                let kind = RowKind::of(p.pattern, my);
                match p.demosaic {
                    Demosaic::Bilinear => {
                        let s = self.window::<3>(p, src, g, my);
                        let rows = s.map(|s| &self.slots[s][col - 1..]);
                        let [r, g, b] = &mut self.rgb16;
                        simd::demosaic_bilinear_row(rows, [r, g, b], n, kind);
                    }
                    Demosaic::Mhc => {
                        let s = self.window::<5>(p, src, g, my);
                        let rows = s.map(|s| &self.slots[s][col - 2..]);
                        let [r, g, b] = &mut self.rgb16;
                        simd::demosaic_mhc_row(rows, [r, g, b], n, kind);
                    }
                }
            }
            Sample::Quads { .. } => {
                let q = self.quad_rows(p, src, g, oy);
                let (t, b) = q.rows(&self.slots, &self.dec, col);
                let [rr, gg, bb] = &mut self.rgb16;
                simd::quad_rgb_row(t, b, [rr, gg, bb], n, p.pattern);
            }
        }
        let [r, g, b] = &mut self.rgb16;
        if let Some(m) = &ip.ccm {
            simd::ccm_row([r, g, b], m, n);
        }
        for (src16, dst8) in self.rgb16.iter().zip(self.rgb8[k].iter_mut()) {
            tone(ip, src16, dst8, n);
        }
        let planes = self.rgb8[k].each_ref().map(|v| &v[..]);
        match dest {
            Dest::Packed(d) => drop(simd::interleave_rgb_row(planes, d, n)),
            Dest::Luma(y) => drop(simd::rgb_to_y_row(planes, y, n, &p.yuv)),
            Dest::LumaChroma(..) => unreachable!("the integer path makes chroma from planes"),
        }
    }

    /// Output row `oy` as 8-bit luma into `dst`.
    fn luma_row(&mut self, p: &Prepared, src: &Source, g: &Geom, oy: usize, dst: &mut [u8]) {
        let n = g.ow;
        let my = Self::mosaic_row(g, oy);
        let col = g.col();
        if let Arith::Half(h) = &p.arith {
            match g.sample {
                Sample::Full => {
                    let s = self.window::<3>(p, src, g, my);
                    let rows = s.map(|s| &self.slots[s][col - 1..]);
                    half::luma_row(rows, dst, n, &h.tone);
                }
                Sample::Quads { .. } => {
                    let q = self.quad_rows(p, src, g, oy);
                    let (t, b) = q.rows(&self.slots, &self.dec, col);
                    half::quad_luma_row(t, b, dst, n, &h.tone);
                }
            }
            return;
        }
        let Arith::Int(ip) = &p.arith else {
            unreachable!()
        };
        match g.sample {
            Sample::Full => {
                let s = self.window::<3>(p, src, g, my);
                let rows = s.map(|s| &self.slots[s][col - 1..]);
                simd::bayer_luma_row(rows, &mut self.luma16, n);
            }
            Sample::Quads { .. } => {
                let q = self.quad_rows(p, src, g, oy);
                let (t, b) = q.rows(&self.slots, &self.dec, col);
                simd::quad_luma_row(t, b, &mut self.luma16, n);
            }
        }
        tone(ip, &self.luma16, dst, n);
    }

    /// Statistics of the quad row starting at mosaic row `y` (even), when sampled. `g` spans
    /// the frame's width.
    pub fn stats_pair(&mut self, p: &Prepared, src: &Source, g: &Geom, y: usize) {
        let (Some(setup), Some(_)) = (&p.stats, &self.stats) else {
            return;
        };
        let qy = y / 2;
        if !qy.is_multiple_of(setup.config.row_step as usize) {
            return;
        }
        debug_assert!(g.fx0 == 0 && g.fw == p.width);
        let (t, b) = (
            self.front(p, src, g, y as isize),
            self.front(p, src, g, y as isize + 1),
        );
        let [r, g, bl] = &mut self.quad;
        let (t, b) = (&self.slots[t][PAD..], &self.slots[b][PAD..]);
        match &p.arith {
            Arith::Int(_) => drop(simd::quad_rgb_row(t, b, [r, g, bl], p.width / 2, p.pattern)),
            Arith::Half(h) => {
                half::quad_stats_row(t, b, [r, g, bl], p.width / 2, p.pattern, h.stats_scale)
            }
        }
        let acc = self.stats.as_mut().expect("checked above");
        acc.add_row(setup, qy, [&self.quad[0], &self.quad[1], &self.quad[2]]);
    }

    /// Statistics for the source rows output row `oy` completes.
    fn stats_after(&mut self, p: &Prepared, src: &Source, g: &Geom, oy: usize) {
        if !g.stats {
            return;
        }
        let my = Self::mosaic_row(g, oy);
        match g.sample {
            Sample::Full if my % 2 == 1 => self.stats_pair(p, src, g, my - 1),
            Sample::Full => {}
            Sample::Quads { step } => {
                for q in 0..step {
                    let y = my + 2 * q;
                    if y + 1 < p.height {
                        self.stats_pair(p, src, g, y);
                    }
                }
            }
        }
    }

    /// Start a band (or a pass of its own): nothing cached from another part of the frame.
    pub fn begin_band(&mut self) {
        self.tags = [None; SLOTS];
        self.stage.clear();
    }

    /// Output rows `o0 .. o0 + rows` of `g` into `out`, whose first row is `o0`.
    pub fn run_band(
        &mut self,
        p: &Prepared,
        src: &Source,
        g: &Geom,
        o0: usize,
        rows: usize,
        out: OutputBuffers,
    ) {
        self.begin_band();
        let ow = g.ow;
        match out {
            OutputBuffers::Luma { data, stride } => {
                for i in 0..rows {
                    self.luma_row(p, src, g, o0 + i, &mut data[i * stride..][..ow]);
                    self.stats_after(p, src, g, o0 + i);
                }
            }
            OutputBuffers::Rgb24 { data, stride } => {
                for i in 0..rows {
                    let row = &mut data[i * stride..][..3 * ow];
                    self.colour_row(p, src, g, o0 + i, 0, Dest::Packed(row));
                    self.stats_after(p, src, g, o0 + i);
                }
            }
            OutputBuffers::Nv12 {
                y,
                y_stride,
                uv,
                uv_stride,
            } => {
                self.yuv_rows(p, src, g, (o0, rows), (y, y_stride), (uv, uv_stride), None);
            }
            OutputBuffers::I420 {
                y,
                y_stride,
                u,
                u_stride,
                v,
                v_stride,
            } => {
                self.yuv_rows(
                    p,
                    src,
                    g,
                    (o0, rows),
                    (y, y_stride),
                    (u, u_stride),
                    Some((v, v_stride)),
                );
            }
        }
    }

    /// 4:2:0 output: rows in pairs; `v` is `None` for NV12 (UV interleaved in `u`).
    #[allow(clippy::too_many_arguments)]
    fn yuv_rows(
        &mut self,
        p: &Prepared,
        src: &Source,
        g: &Geom,
        (o0, rows): (usize, usize),
        (y, y_stride): (&mut [u8], usize),
        (u, u_stride): (&mut [u8], usize),
        mut v: Option<(&mut [u8], usize)>,
    ) {
        let ow = g.ow;
        for i in (0..rows).step_by(2) {
            let (y0, y1) = y[i * y_stride..].split_at_mut(y_stride);
            let (y0, y1) = (&mut y0[..ow], &mut y1[..ow]);
            let u_row = &mut u[i / 2 * u_stride..];
            if matches!(p.arith, Arith::Half(_)) {
                // fp16: the second row's kernel makes the pair's chroma. Each row's statistics
                // straight after it, while its front rows are still in the ring.
                self.colour_row(p, src, g, o0 + i, 0, Dest::Luma(y0));
                self.stats_after(p, src, g, o0 + i);
                let (u_row, v_row) = match &mut v {
                    None => (&mut u_row[..ow], None),
                    Some((v, v_stride)) => (
                        &mut u_row[..ow / 2],
                        Some(&mut v[i / 2 * *v_stride..][..ow / 2]),
                    ),
                };
                let dest = Dest::LumaChroma(y1, u_row, v_row);
                self.colour_row(p, src, g, o0 + i + 1, 1, dest);
            } else {
                self.colour_row(p, src, g, o0 + i, 0, Dest::Luma(y0));
                self.stats_after(p, src, g, o0 + i);
                self.colour_row(p, src, g, o0 + i + 1, 1, Dest::Luma(y1));
                let top = self.rgb8[0].each_ref().map(|v| &v[..]);
                let bottom = self.rgb8[1].each_ref().map(|v| &v[..]);
                match &mut v {
                    None => drop(simd::rgb_to_uv_row(
                        top,
                        bottom,
                        u_row,
                        &mut [],
                        ow / 2,
                        &p.yuv,
                        true,
                    )),
                    Some((v, v_stride)) => {
                        let v_row = &mut v[i / 2 * *v_stride..];
                        simd::rgb_to_uv_row(top, bottom, u_row, v_row, ow / 2, &p.yuv, false);
                    }
                }
            }
            self.stats_after(p, src, g, o0 + i + 1);
        }
    }
}

/// Where an output row's quads are.
#[derive(Clone, Copy)]
enum Quads {
    /// The top and bottom front rows' slots.
    Slots(usize, usize),
    /// Gathered into `Worker::dec`.
    Gathered,
}

impl Quads {
    fn rows<'a>(
        self,
        slots: &'a [Vec<u16>],
        dec: &'a [Vec<u16>; 2],
        col: usize,
    ) -> (&'a [u16], &'a [u16]) {
        match self {
            Self::Slots(t, b) => (&slots[t][col..], &slots[b][col..]),
            Self::Gathered => (&dec[0][..], &dec[1][..]),
        }
    }
}

fn tone(p: &IntPrep, src: &[u16], dst: &mut [u8], n: usize) {
    match (&p.poly, &p.lut) {
        // Quadratics exist only with feature `poly-tone`: without it the arm (and the kernel)
        // is dead code.
        (Some(poly), _) if cfg!(feature = "poly-tone") => drop(simd::poly_row(src, dst, poly, n)),
        (_, Some(lut)) => drop(simd::lut_row(src, dst, lut, n)),
        (_, None) => drop(simd::narrow_row(src, dst, n)),
    }
}
