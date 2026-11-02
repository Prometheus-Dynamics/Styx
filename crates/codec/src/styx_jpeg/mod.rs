//! Styx's own baseline JPEG encoder (pure Rust): JFIF colour and the DCT with quantisation from
//! `styx_core::simd::jpeg` (RVV leaves on RISC-V, the scalar oracle elsewhere), Huffman coding
//! with the typical tables of T.81 Annex K. 4:2:0 for colour, one component for grey.
//!
//! The image is coded one MCU row at a time: the row's samples are padded to whole MCUs (edges
//! repeated) into strips kept between images, so a steady stream of one size allocates nothing.

mod codec;
mod entropy;
#[cfg(all(feature = "gpu-jpeg", target_os = "linux"))]
mod gpu;
mod tables;
#[cfg(test)]
mod tests;

pub use codec::StyxJpegEncoder;
use entropy::{BitWriter, HuffCodes};
#[cfg(all(feature = "gpu-jpeg", target_os = "linux"))]
pub use gpu::GpuStyxJpeg;
use styx_core::simd::jpeg::{fdct_quant_band, rgb_ycc420_rows};
use styx_core::simd::{ColorLayout, SimdBackend};
use tables::{HUFF, ZIGZAG, quant_tables};

use crate::CodecError;

/// Pixels to encode. Planes hold `height` rows `stride` apart; 4:2:0 chroma is
/// `width.div_ceil(2)` x `height.div_ceil(2)`.
#[derive(Clone, Copy, Debug)]
pub enum StyxJpegInput<'a> {
    /// Packed RGB or BGR, 3 or 4 bytes a pixel (JFIF colour conversion).
    Packed {
        data: &'a [u8],
        width: usize,
        height: usize,
        stride: usize,
        layout: ColorLayout,
    },
    /// Planar YUV 4:2:0, taken as JFIF (full-range BT.601) YCbCr.
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

impl StyxJpegInput<'_> {
    pub fn size(&self) -> (usize, usize) {
        match *self {
            Self::Packed { width, height, .. }
            | Self::I420 { width, height, .. }
            | Self::Nv12 { width, height, .. }
            | Self::Gray { width, height, .. } => (width, height),
        }
    }

    fn check(&self) -> Result<(), CodecError> {
        let fits = |plane: &[u8], stride: usize, row: usize, rows: usize| {
            stride >= row && plane.len() >= stride * (rows - 1) + row
        };
        let (w, h) = self.size();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let ok = w > 0
            && h > 0
            && w <= 65535
            && h <= 65535
            && match *self {
                Self::Packed {
                    data,
                    stride,
                    layout,
                    ..
                } => fits(data, stride, w * layout.bytes_per_pixel(), h),
                Self::I420 {
                    y,
                    u,
                    v,
                    y_stride,
                    c_stride,
                    ..
                } => {
                    fits(y, y_stride, w, h)
                        && fits(u, c_stride, cw, ch)
                        && fits(v, c_stride, cw, ch)
                }
                Self::Nv12 {
                    y,
                    uv,
                    y_stride,
                    uv_stride,
                    ..
                } => fits(y, y_stride, w, h) && fits(uv, uv_stride, 2 * cw, ch),
                Self::Gray { y, stride, .. } => fits(y, stride, w, h),
            };
        if ok {
            Ok(())
        } else {
            Err(CodecError::Codec(
                "jpeg input: empty, over 65535 or planes smaller than their size".into(),
            ))
        }
    }
}

/// The encoder: tables for one quality and the strips of one MCU row, kept between images.
pub struct StyxJpeg {
    quality: u8,
    quant: [[u16; 64]; 2],
    huff: [HuffCodes; 4],
    /// One MCU row of Y, Cb and Cr samples, padded to whole MCUs.
    strips: [Vec<u8>; 3],
    /// Their quantised coefficients, 64 a block, natural order.
    coefs: [Vec<i16>; 3],
    backend: SimdBackend,
}

impl std::fmt::Debug for StyxJpeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StyxJpeg")
            .field("quality", &self.quality)
            .field("backend", &self.backend)
            .finish()
    }
}

impl StyxJpeg {
    /// An encoder at `quality` (1–100, libjpeg's scale).
    pub fn new(quality: u8) -> Self {
        let quality = quality.clamp(1, 100);
        Self {
            quality,
            quant: quant_tables(quality),
            huff: HUFF.each_ref().map(HuffCodes::new),
            strips: Default::default(),
            coefs: Default::default(),
            backend: SimdBackend::Scalar,
        }
    }

    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// The backend of the last image's DCT.
    pub fn backend(&self) -> SimdBackend {
        self.backend
    }

    /// Encode `input` into `out` (cleared first; its capacity reused).
    pub fn encode(
        &mut self,
        input: &StyxJpegInput<'_>,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        input.check()?;
        out.clear();
        let (w, h) = input.size();
        let gray = matches!(input, StyxJpegInput::Gray { .. });
        // MCU: 16x16 (four luma blocks, one each of Cb and Cr) or 8x8 grey.
        let mcu = if gray { 8 } else { 16 };
        let (pw, rows) = (w.next_multiple_of(mcu), h.div_ceil(mcu));
        self.headers(w, h, gray, out);
        let cw = pw / 2;
        self.strips[0].resize(pw * mcu, 0);
        self.coefs[0].resize(pw * mcu, 0);
        if !gray {
            for c in 1..3 {
                self.strips[c].resize(cw * 8, 0);
                self.coefs[c].resize(cw * 8, 0);
            }
        }
        let mut preds = [0i16; 3];
        let mut bits = BitWriter::new(out);
        for row in 0..rows {
            self.fill(input, row, pw);
            let [yq, cq] = &self.quant;
            let mut backend = SimdBackend::Scalar;
            for band in 0..mcu / 8 {
                backend = fdct_quant_band(
                    &self.strips[0][band * 8 * pw..],
                    pw,
                    pw / 8,
                    yq,
                    &mut self.coefs[0][band * 8 * pw..],
                );
            }
            if !gray {
                for c in 1..3 {
                    fdct_quant_band(&self.strips[c], cw, cw / 8, cq, &mut self.coefs[c]);
                }
            }
            self.backend = backend;
            let chroma = (!gray).then(|| (&self.coefs[1][..], &self.coefs[2][..]));
            code_mcu_row(
                &mut bits,
                &self.huff,
                &mut preds,
                &self.coefs[0],
                chroma,
                pw / 8,
            );
        }
        bits.flush();
        out.extend_from_slice(&[0xFF, 0xD9]);
        Ok(())
    }

    /// The quantisers (luma, chroma; natural order), for computing blocks elsewhere (the GPU's
    /// `styx_gpuisp::GpuConvert::jpeg_blocks`).
    pub fn quant(&self) -> &[[u16; 64]; 2] {
        &self.quant
    }

    /// Codes blocks computed elsewhere with [`Self::quant`]: `blocks` laid out as
    /// `styx_gpuisp::JpegGrid` has them for a `w` x `h` image (`width` = padded width / 8
    /// blocks a row; the luma block rows, padded to whole MCUs; with chroma, then one row per
    /// chroma block row, its Cb blocks then its Cr blocks).
    pub fn encode_blocks(
        &mut self,
        (w, h): (usize, usize),
        gray: bool,
        blocks: &[i16],
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        let mcu = if gray { 8 } else { 16 };
        let (pw, ph) = (w.next_multiple_of(mcu), h.next_multiple_of(mcu));
        let (bw, luma_rows) = (pw / 8, ph / 8);
        let rows = luma_rows + if gray { 0 } else { ph / 16 };
        if !(1..=65535).contains(&w) || !(1..=65535).contains(&h) || blocks.len() < bw * rows * 64 {
            return Err(CodecError::Codec(format!(
                "jpeg blocks: {} coefficients for a {w}x{h} grid",
                blocks.len()
            )));
        }
        out.clear();
        self.headers(w, h, gray, out);
        let mut preds = [0i16; 3];
        let mut bits = BitWriter::new(out);
        let band = bw * 64;
        for row in 0..ph / mcu {
            let luma = &blocks[row * (mcu / 8) * band..][..(mcu / 8) * band];
            let chroma =
                (!gray).then(|| blocks[(luma_rows + row) * band..][..band].split_at(band / 2));
            code_mcu_row(&mut bits, &self.huff, &mut preds, luma, chroma, bw);
        }
        bits.flush();
        out.extend_from_slice(&[0xFF, 0xD9]);
        Ok(())
    }

    /// MCU row `row`'s samples into the strips (`pw` luma columns), edges repeated.
    fn fill(&mut self, input: &StyxJpegInput<'_>, row: usize, pw: usize) {
        let (w, h) = input.size();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let [ys, cbs, crs] = &mut self.strips;
        let pcw = pw / 2;
        match *input {
            StyxJpegInput::Gray { y, stride, .. } => {
                copy_rows(y, stride, (w, h), 8 * row, 8, ys, pw);
            }
            StyxJpegInput::I420 {
                y,
                u,
                v,
                y_stride,
                c_stride,
                ..
            } => {
                copy_rows(y, y_stride, (w, h), 16 * row, 16, ys, pw);
                copy_rows(u, c_stride, (cw, ch), 8 * row, 8, cbs, pcw);
                copy_rows(v, c_stride, (cw, ch), 8 * row, 8, crs, pcw);
            }
            StyxJpegInput::Nv12 {
                y,
                uv,
                y_stride,
                uv_stride,
                ..
            } => {
                copy_rows(y, y_stride, (w, h), 16 * row, 16, ys, pw);
                for r in 0..8 {
                    let src = &uv[(8 * row + r).min(ch - 1) * uv_stride..][..2 * cw];
                    let (cb, cr) = (&mut cbs[r * pcw..][..pcw], &mut crs[r * pcw..][..pcw]);
                    for (i, &[u, v]) in src.as_chunks::<2>().0.iter().enumerate() {
                        (cb[i], cr[i]) = (u, v);
                    }
                    pad_row(cb, cw);
                    pad_row(cr, cw);
                }
            }
            StyxJpegInput::Packed {
                data,
                stride,
                layout,
                ..
            } => {
                let line =
                    |r: usize| &data[r.min(h - 1) * stride..][..w * layout.bytes_per_pixel()];
                for p in 0..8 {
                    let r = 16 * row + 2 * p;
                    let (yt, yb) = ys[2 * p * pw..(2 * p + 2) * pw].split_at_mut(pw);
                    let (cb, cr) = (&mut cbs[p * pcw..][..pcw], &mut crs[p * pcw..][..pcw]);
                    // Rows past the end repeat the last one (as do the 2x2 sums of an odd height).
                    rgb_ycc420_rows(line(r), line(r + 1), yt, yb, cb, cr, w, layout);
                    for row in [yt, yb, cb, cr] {
                        let n = if row.len() == pw { w } else { cw };
                        pad_row(row, n);
                    }
                }
            }
        }
    }

    fn headers(&self, w: usize, h: usize, gray: bool, out: &mut Vec<u8>) {
        let tables = if gray { 1 } else { 2 };
        out.extend_from_slice(&[0xFF, 0xD8]);
        // APP0 JFIF 1.01, no density, no thumbnail.
        out.extend_from_slice(&[
            0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0,
        ]);
        segment(out, 0xDB, 65 * tables);
        for (id, table) in self.quant[..tables].iter().enumerate() {
            out.push(id as u8);
            out.extend(ZIGZAG.iter().map(|&k| table[usize::from(k)] as u8));
        }
        let comps = if gray { 1 } else { 3 };
        segment(out, 0xC0, 6 + 3 * comps);
        out.push(8);
        out.extend_from_slice(&(h as u16).to_be_bytes());
        out.extend_from_slice(&(w as u16).to_be_bytes());
        out.push(comps as u8);
        if gray {
            out.extend_from_slice(&[1, 0x11, 0]);
        } else {
            out.extend_from_slice(&[1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        }
        let specs = &HUFF[..2 * tables];
        segment(
            out,
            0xC4,
            specs.iter().map(|s| 17 + s.vals.len()).sum::<usize>(),
        );
        for (i, spec) in specs.iter().enumerate() {
            // Class (0 DC, 1 AC) and table id.
            out.push((((i & 1) << 4) | (i >> 1)) as u8);
            out.extend_from_slice(&spec.bits);
            out.extend_from_slice(spec.vals);
        }
        segment(out, 0xDA, 4 + 2 * comps);
        out.push(comps as u8);
        if gray {
            out.extend_from_slice(&[1, 0x00]);
        } else {
            out.extend_from_slice(&[1, 0x00, 2, 0x11, 3, 0x11]);
        }
        out.extend_from_slice(&[0, 63, 0]);
    }
}

/// One MCU row: `luma` holds one band (grey) or two of `bw` blocks each, `chroma` the Cb and
/// Cr bands of `bw / 2` blocks.
fn code_mcu_row(
    bits: &mut BitWriter<'_>,
    huff: &[HuffCodes; 4],
    preds: &mut [i16; 3],
    luma: &[i16],
    chroma: Option<(&[i16], &[i16])>,
    bw: usize,
) {
    let [ldc, lac, cdc, cac] = huff;
    let Some((cb, cr)) = chroma else {
        for x in 0..bw {
            bits.block(block(luma, x), &mut preds[0], ldc, lac);
        }
        return;
    };
    // Blocks of the top luma band, then those of the bottom one.
    let (top, bottom) = luma.split_at(bw * 64);
    for x in 0..bw / 2 {
        for band in [top, bottom] {
            bits.block(block(band, 2 * x), &mut preds[0], ldc, lac);
            bits.block(block(band, 2 * x + 1), &mut preds[0], ldc, lac);
        }
        bits.block(block(cb, x), &mut preds[1], cdc, cac);
        bits.block(block(cr, x), &mut preds[2], cdc, cac);
    }
}

/// Block `i` of a band's coefficients.
fn block(coefs: &[i16], i: usize) -> &[i16] {
    &coefs[64 * i..64 * i + 64]
}

/// A marker and the length field of a segment of `len` bytes after it.
fn segment(out: &mut Vec<u8>, marker: u8, len: usize) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((len + 2) as u16).to_be_bytes());
}

/// Rows `first..first + rows` of `src` (`w` x `h`, rows `stride` apart) into `dst` (rows `pw`
/// apart), the last row and column repeated past the edges.
fn copy_rows(
    src: &[u8],
    stride: usize,
    (w, h): (usize, usize),
    first: usize,
    rows: usize,
    dst: &mut [u8],
    pw: usize,
) {
    for r in 0..rows {
        let out = &mut dst[r * pw..][..pw];
        out[..w].copy_from_slice(&src[(first + r).min(h - 1) * stride..][..w]);
        pad_row(out, w);
    }
}

/// Repeats `row[n - 1]` over the rest of `row`.
fn pad_row(row: &mut [u8], n: usize) {
    let last = row[n - 1];
    row[n..].fill(last);
}
