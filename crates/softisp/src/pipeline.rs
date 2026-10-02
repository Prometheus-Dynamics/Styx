//! The row loop: a worker turns a band of output rows into pixels, keeping a small ring of
//! front-end rows (unpacked, black level, gains, lens shading) around the current row.

use crate::format::RawPacking;
use crate::output::{OutputBuffers, Scale};
use crate::params::Demosaic;
use crate::prepare::Prepared;
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
}

impl Source<'_> {
    /// Input row `y`, from the staging copy (refilled from `y` on a miss).
    fn row<'s>(&self, stage: &'s mut Stage, y: usize, height: usize, bytes: usize) -> &'s [u8] {
        if !(stage.first..stage.first + stage.rows).contains(&y) {
            let rows = (STAGE_BYTES / self.stride).clamp(1, height - y);
            let len = self.stride * (rows - 1) + bytes;
            stage.buf.resize(len.max(stage.buf.len()), 0);
            stage.buf[..len].copy_from_slice(&self.data[y * self.stride..][..len]);
            stage.first = y;
            stage.rows = rows;
        }
        &stage.buf[(y - stage.first) * self.stride..][..bytes]
    }

    fn unpack(&self, stage: &mut Stage, y: usize, height: usize, width: usize, dst: &mut [u16]) {
        let row = self.row(stage, y, height, self.packing.row_bytes(width));
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
    stage: Stage,
    pub stats: Option<StatsAccum>,
}

impl Worker {
    /// Size the scratch for `p` and reset the statistics, at the start of a frame.
    pub fn begin_frame(&mut self, p: &Prepared) {
        let w = p.width;
        if self.width != w {
            self.width = w;
            self.slots = vec![vec![0; w + 2 * PAD]; SLOTS];
            self.rgb16 = std::array::from_fn(|_| vec![0; w]);
            self.rgb8 = std::array::from_fn(|_| std::array::from_fn(|_| vec![0; w]));
            self.luma16 = vec![0; w];
            self.quad = std::array::from_fn(|_| vec![0; w / 2]);
        }
        match (&p.stats, &mut self.stats) {
            (Some(setup), Some(acc))
                if acc.histogram.len() == 4 * setup.config.histogram_bins as usize =>
            {
                acc.reset()
            }
            (Some(setup), acc) => *acc = Some(StatsAccum::new(setup)),
            (None, acc) => *acc = None,
        }
    }

    /// The slot holding front row `y` (reflected into the frame), computing it if needed.
    fn front(&mut self, p: &Prepared, src: &Source, y: isize) -> usize {
        let ry = reflect(y, p.height);
        let slot = ry % SLOTS;
        if self.tags[slot] == Some(ry) {
            return slot;
        }
        let w = p.width;
        let row = &mut self.slots[slot];
        src.unpack(&mut self.stage, ry, p.height, w, &mut row[PAD..PAD + w]);
        let gains = p.gains.row(ry, &mut self.gains);
        simd::front_row(&mut row[PAD..], p.black[ry & 1], gains, p.shift, w);
        for k in 1..=PAD {
            row[PAD - k] = row[PAD + k];
            row[PAD + w - 1 + k] = row[PAD + w - 1 - k];
        }
        self.tags[slot] = Some(ry);
        slot
    }

    /// Front rows `y - r ..= y + r`, each from column `-r`.
    fn window<const N: usize>(&mut self, p: &Prepared, src: &Source, y: usize) -> [usize; N] {
        let r = (N / 2) as isize;
        std::array::from_fn(|i| self.front(p, src, y as isize - r + i as isize))
    }

    /// Output row `oy` as planar 8-bit RGB in `rgb8[k]`.
    fn colour_row(&mut self, p: &Prepared, src: &Source, scale: Scale, oy: usize, k: usize) {
        let w = p.width;
        let n = match scale {
            Scale::Full => {
                let kind = RowKind::of(p.pattern, oy);
                match p.demosaic {
                    Demosaic::Bilinear => {
                        let s = self.window::<3>(p, src, oy);
                        let rows = s.map(|s| &self.slots[s][PAD - 1..]);
                        let [r, g, b] = &mut self.rgb16;
                        simd::demosaic_bilinear_row(rows, [r, g, b], w, kind);
                    }
                    Demosaic::Mhc => {
                        let s = self.window::<5>(p, src, oy);
                        let rows = s.map(|s| &self.slots[s][PAD - 2..]);
                        let [r, g, b] = &mut self.rgb16;
                        simd::demosaic_mhc_row(rows, [r, g, b], w, kind);
                    }
                }
                w
            }
            Scale::Half => {
                let (t, b) = (
                    self.front(p, src, 2 * oy as isize),
                    self.front(p, src, 2 * oy as isize + 1),
                );
                let [rr, gg, bb] = &mut self.rgb16;
                simd::quad_rgb_row(
                    &self.slots[t][PAD..],
                    &self.slots[b][PAD..],
                    [rr, gg, bb],
                    w / 2,
                    p.pattern,
                );
                w / 2
            }
        };
        let [r, g, b] = &mut self.rgb16;
        if let Some(m) = &p.ccm {
            simd::ccm_row([r, g, b], m, n);
        }
        for (src16, dst8) in self.rgb16.iter().zip(self.rgb8[k].iter_mut()) {
            tone(p, src16, dst8, n);
        }
    }

    /// Output row `oy` as 8-bit luma into `dst`.
    fn luma_row(&mut self, p: &Prepared, src: &Source, scale: Scale, oy: usize, dst: &mut [u8]) {
        let w = p.width;
        let n = match scale {
            Scale::Full => {
                let s = self.window::<3>(p, src, oy);
                let rows = s.map(|s| &self.slots[s][PAD - 1..]);
                simd::bayer_luma_row(rows, &mut self.luma16, w);
                w
            }
            Scale::Half => {
                let (t, b) = (
                    self.front(p, src, 2 * oy as isize),
                    self.front(p, src, 2 * oy as isize + 1),
                );
                simd::quad_luma_row(
                    &self.slots[t][PAD..],
                    &self.slots[b][PAD..],
                    &mut self.luma16,
                    w / 2,
                );
                w / 2
            }
        };
        tone(p, &self.luma16, dst, n);
    }

    /// Statistics of the quad row starting at mosaic row `y` (even), when sampled.
    fn stats_pair(&mut self, p: &Prepared, src: &Source, y: usize) {
        let (Some(setup), Some(_)) = (&p.stats, &self.stats) else {
            return;
        };
        let qy = y / 2;
        if !qy.is_multiple_of(setup.config.row_step as usize) {
            return;
        }
        let (t, b) = (
            self.front(p, src, y as isize),
            self.front(p, src, y as isize + 1),
        );
        let [r, g, bl] = &mut self.quad;
        simd::quad_rgb_row(
            &self.slots[t][PAD..],
            &self.slots[b][PAD..],
            [r, g, bl],
            p.width / 2,
            p.pattern,
        );
        let acc = self.stats.as_mut().expect("checked above");
        acc.add_row(setup, qy, [&self.quad[0], &self.quad[1], &self.quad[2]]);
    }

    /// Statistics for the source rows output row `oy` completes.
    fn stats_after(&mut self, p: &Prepared, src: &Source, scale: Scale, oy: usize) {
        match scale {
            Scale::Full if oy % 2 == 1 => self.stats_pair(p, src, oy - 1),
            Scale::Full => {}
            Scale::Half => self.stats_pair(p, src, 2 * oy),
        }
    }

    /// Output rows `o0 .. o0 + rows` into `out`, whose first row is `o0`.
    pub fn run_band(
        &mut self,
        p: &Prepared,
        src: &Source,
        scale: Scale,
        o0: usize,
        rows: usize,
        out: OutputBuffers,
    ) {
        self.tags = [None; SLOTS];
        self.stage.rows = 0;
        let ow = match scale {
            Scale::Full => p.width,
            Scale::Half => p.width / 2,
        };
        match out {
            OutputBuffers::Luma { data, stride } => {
                for i in 0..rows {
                    self.luma_row(p, src, scale, o0 + i, &mut data[i * stride..][..ow]);
                    self.stats_after(p, src, scale, o0 + i);
                }
            }
            OutputBuffers::Rgb24 { data, stride } => {
                for i in 0..rows {
                    self.colour_row(p, src, scale, o0 + i, 0);
                    let planes = self.rgb8[0].each_ref().map(|v| &v[..]);
                    simd::interleave_rgb_row(planes, &mut data[i * stride..], ow);
                    self.stats_after(p, src, scale, o0 + i);
                }
            }
            OutputBuffers::Nv12 {
                y,
                y_stride,
                uv,
                uv_stride,
            } => {
                self.yuv_rows(
                    p,
                    src,
                    scale,
                    (o0, rows, ow),
                    (y, y_stride),
                    (uv, uv_stride),
                    None,
                );
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
                    scale,
                    (o0, rows, ow),
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
        scale: Scale,
        (o0, rows, ow): (usize, usize, usize),
        (y, y_stride): (&mut [u8], usize),
        (u, u_stride): (&mut [u8], usize),
        mut v: Option<(&mut [u8], usize)>,
    ) {
        for i in (0..rows).step_by(2) {
            self.colour_row(p, src, scale, o0 + i, 0);
            self.colour_row(p, src, scale, o0 + i + 1, 1);
            for k in 0..2 {
                let planes = self.rgb8[k].each_ref().map(|v| &v[..]);
                simd::rgb_to_y_row(planes, &mut y[(i + k) * y_stride..], ow, &p.yuv);
            }
            let top = self.rgb8[0].each_ref().map(|v| &v[..]);
            let bottom = self.rgb8[1].each_ref().map(|v| &v[..]);
            let u_row = &mut u[i / 2 * u_stride..];
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
            self.stats_after(p, src, scale, o0 + i);
            self.stats_after(p, src, scale, o0 + i + 1);
        }
    }
}

fn tone(p: &Prepared, src: &[u16], dst: &mut [u8], n: usize) {
    match &p.lut {
        Some(lut) => drop(simd::lut_row(src, dst, lut, n)),
        None => drop(simd::narrow_row(src, dst, n)),
    }
}
