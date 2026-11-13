//! The encoder: planar samples to quantised 8x8 blocks (styx-simd's colour-free DCT and
//! quantisation kernels) to Huffman-coded JFIF bytes.
//!
//! The image is coded one MCU row (a slice, 16 luma rows, or 8 for grey) at a time. A row whose
//! samples are whole in the caller's planes is transformed in place; only a row that needs edge
//! padding, or NV12's interleaved chroma, is copied into a strip kept between images. A steady
//! stream of one size therefore allocates nothing and copies only what the format forces.
//!
//! The two stages are separate so that a GPU can supply the middle one: [`JpegEncoder::quant`]
//! gives the quantisers, [`JpegEncoder::encode_blocks`] codes blocks computed elsewhere in the
//! [`CoefficientGrid`] layout. The GPU kernels that match these bytes are a later change.

use styx_codec_api::CodecError;
use styx_simd::SimdBackend;
use styx_simd::jpeg::fdct_quant_band;

use crate::entropy::{BitWriter, ByteSink, Fixed, HuffCodes};
use crate::tables::{HUFF, ZIGZAG, quant_tables};

/// Planar pixels to encode. Each plane holds its rows `stride` bytes apart; chroma of 4:2:0 is
/// `width.div_ceil(2)` by `height.div_ceil(2)`. Colour is JFIF (full-range BT.601) YCbCr.
#[derive(Clone, Copy, Debug)]
pub enum PlanarInput<'a> {
    /// Three planes: Y, then Cb and Cr.
    I420 {
        y: &'a [u8],
        u: &'a [u8],
        v: &'a [u8],
        width: usize,
        height: usize,
        y_stride: usize,
        c_stride: usize,
    },
    /// Two planes: Y, then Cb and Cr interleaved (`uv` rows hold `2 * width.div_ceil(2)` bytes).
    Nv12 {
        y: &'a [u8],
        uv: &'a [u8],
        width: usize,
        height: usize,
        y_stride: usize,
        uv_stride: usize,
    },
    /// One plane of 8-bit grey (one component, 8x8 MCUs).
    Gray {
        y: &'a [u8],
        width: usize,
        height: usize,
        stride: usize,
    },
}

impl PlanarInput<'_> {
    /// Width and height in pixels.
    pub fn size(&self) -> (usize, usize) {
        match *self {
            Self::I420 { width, height, .. }
            | Self::Nv12 { width, height, .. }
            | Self::Gray { width, height, .. } => (width, height),
        }
    }

    fn is_gray(&self) -> bool {
        matches!(self, Self::Gray { .. })
    }

    /// Checks the sizes, and that every plane holds the rows its stride and size say.
    pub fn check(&self) -> Result<(), CodecError> {
        let fits = |plane: &[u8], stride: usize, row: usize, rows: usize| {
            stride >= row && plane.len() >= stride * (rows - 1) + row
        };
        let (w, h) = self.size();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let ok = (1..=65535).contains(&w)
            && (1..=65535).contains(&h)
            && match *self {
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
            Err(CodecError::Rejected(
                "jpeg input: size outside 1..=65535, or planes smaller than their size".into(),
            ))
        }
    }
}

/// Where the blocks of an image are, for [`JpegEncoder::encode_blocks`]: `width` blocks a row,
/// row-major, 64 coefficients a block (natural order). The first `luma_rows` block rows are
/// luma, padded to whole MCUs (16x16 with chroma, 8x8 grey). With chroma, each later block row
/// holds the Cb blocks of one chroma block row, then its Cr blocks (`width / 2` each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoefficientGrid {
    pub width: usize,
    pub luma_rows: usize,
    pub rows: usize,
}

impl CoefficientGrid {
    /// The grid of a `(w, h)` image: colour, or grey when `gray`.
    pub fn new((w, h): (usize, usize), gray: bool) -> Self {
        let mcu = if gray { 8 } else { 16 };
        let (pw, ph) = (w.next_multiple_of(mcu), h.next_multiple_of(mcu));
        Self {
            width: pw / 8,
            luma_rows: ph / 8,
            rows: ph / 8 + if gray { 0 } else { ph / 16 },
        }
    }

    /// Blocks in the grid.
    pub fn blocks(&self) -> usize {
        self.width * self.rows
    }
}

/// The encoder: its tables for one quality, and the strips and coefficients of one MCU row, kept
/// between images.
pub struct JpegEncoder {
    quality: u8,
    quant: [[u16; 64]; 2],
    huff: [HuffCodes; 4],
    restart_rows: bool,
    /// One MCU row of Y, Cb and Cr samples, padded to whole MCUs, where padding is needed.
    strips: [Vec<u8>; 3],
    /// Their quantised coefficients, 64 a block, natural order.
    coefs: [Vec<i16>; 3],
    backend: SimdBackend,
}

impl std::fmt::Debug for JpegEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JpegEncoder")
            .field("quality", &self.quality)
            .field("restart_rows", &self.restart_rows)
            .field("backend", &self.backend)
            .finish()
    }
}

/// Where one component's samples of an MCU row are: the first row's bytes and the stride.
#[derive(Clone, Copy)]
struct Samples<'a> {
    data: &'a [u8],
    stride: usize,
}

/// The samples of one MCU row: luma, and Cb and Cr where the format has them.
struct RowSamples<'a> {
    luma: Samples<'a>,
    chroma: Option<[Samples<'a>; 2]>,
}

impl JpegEncoder {
    /// An encoder at `quality` (1 to 100, libjpeg's scale), baseline, one restart-free scan.
    pub fn new(quality: u8) -> Self {
        let quality = quality.clamp(1, 100);
        Self {
            quality,
            quant: quant_tables(quality),
            huff: HUFF.each_ref().map(HuffCodes::new),
            restart_rows: false,
            strips: Default::default(),
            coefs: Default::default(),
            backend: SimdBackend::Scalar,
        }
    }

    /// Emits a restart marker after every MCU row, so each row decodes on its own: the slices a
    /// parallel decoder can split the scan at. Off by default (the bytes then carry no markers).
    pub fn with_restart_rows(mut self, on: bool) -> Self {
        self.restart_rows = on;
        self
    }

    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// Whether restart markers follow each MCU row.
    pub fn restart_rows(&self) -> bool {
        self.restart_rows
    }

    /// The backend of the last row's DCT.
    pub fn backend(&self) -> SimdBackend {
        self.backend
    }

    /// The quantisers (luma, chroma; natural order), for computing blocks elsewhere.
    pub fn quant(&self) -> &[[u16; 64]; 2] {
        &self.quant
    }

    /// Encodes `input` into `out` (cleared first; its capacity reused).
    pub fn encode(&mut self, input: &PlanarInput<'_>, out: &mut Vec<u8>) -> Result<(), CodecError> {
        out.clear();
        self.run(input, out)
    }

    /// Encodes `input` into the start of `out`, returning the bytes written. Fails, writing
    /// nothing useful, when the JPEG does not fit.
    pub fn encode_fixed(
        &mut self,
        input: &PlanarInput<'_>,
        out: &mut [u8],
    ) -> Result<usize, CodecError> {
        let mut sink = Fixed::new(out);
        self.run(input, &mut sink)?;
        sink.written()
            .ok_or_else(|| CodecError::Rejected("the JPEG does not fit the output buffer".into()))
    }

    /// Codes blocks computed elsewhere with [`Self::quant`], laid out as [`CoefficientGrid`]
    /// describes for a `(w, h)` image. The output is the bytes [`Self::encode`] gives for the
    /// pixels those blocks came from.
    pub fn encode_blocks(
        &mut self,
        (w, h): (usize, usize),
        gray: bool,
        blocks: &[i16],
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        out.clear();
        let mcu = if gray { 8 } else { 16 };
        let grid = CoefficientGrid::new((w, h), gray);
        if !(1..=65535).contains(&w)
            || !(1..=65535).contains(&h)
            || blocks.len() < grid.blocks() * 64
        {
            return Err(CodecError::Rejected(format!(
                "jpeg blocks: {} coefficients for a {w}x{h} grid",
                blocks.len()
            )));
        }
        let restart = self.restart_rows.then_some(grid.width / (mcu / 8));
        self.headers(w, h, gray, restart, out);
        let mut preds = [0i16; 3];
        let mut bits = BitWriter::new(out);
        let band = grid.width * 64;
        let mcu_rows = grid.luma_rows / (mcu / 8);
        for row in 0..mcu_rows {
            let luma = &blocks[row * (mcu / 8) * band..][..(mcu / 8) * band];
            let chroma = (!gray).then(|| {
                let (cb, cr) = blocks[(grid.luma_rows + row) * band..][..band].split_at(band / 2);
                (cb, cr)
            });
            code_mcu_row(&mut bits, &self.huff, &mut preds, luma, chroma, grid.width);
            end_row(&mut bits, &mut preds, row, mcu_rows, restart.is_some());
        }
        bits.flush();
        out.extend_from_slice(&[0xFF, 0xD9]);
        Ok(())
    }

    /// Writes the JPEG of `input` to `sink` (the codec adapter's path, into a frame's plane).
    pub(crate) fn run<S: ByteSink>(
        &mut self,
        input: &PlanarInput<'_>,
        sink: &mut S,
    ) -> Result<(), CodecError> {
        input.check()?;
        let (w, h) = input.size();
        let gray = input.is_gray();
        // MCU: 16x16 (four luma blocks, one each of Cb and Cr) or 8x8 grey.
        let mcu = if gray { 8 } else { 16 };
        let (pw, rows) = (w.next_multiple_of(mcu), h.div_ceil(mcu));
        let restart = self.restart_rows.then_some(pw / mcu);
        self.headers(w, h, gray, restart, sink);
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
        let mut bits = BitWriter::new(&mut *sink);
        for row in 0..rows {
            stage_row(input, row, pw, &mut self.strips);
            let samples = row_samples(input, row, pw, &self.strips);
            let [yq, cq] = &self.quant;
            let luma = samples.luma;
            let mut backend = SimdBackend::Scalar;
            for band in 0..mcu / 8 {
                backend = fdct_quant_band(
                    &luma.data[band * 8 * luma.stride..],
                    luma.stride,
                    pw / 8,
                    yq,
                    &mut self.coefs[0][band * 8 * pw..],
                );
            }
            if let Some([cb, cr]) = samples.chroma {
                fdct_quant_band(cb.data, cb.stride, cw / 8, cq, &mut self.coefs[1]);
                fdct_quant_band(cr.data, cr.stride, cw / 8, cq, &mut self.coefs[2]);
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
            end_row(&mut bits, &mut preds, row, rows, restart.is_some());
        }
        bits.flush();
        sink.push_bytes(&[0xFF, 0xD9]);
        Ok(())
    }

    /// SOI, JFIF, the quantisers, the frame, the Huffman tables, the restart interval, and the
    /// scan header.
    fn headers<S: ByteSink>(
        &self,
        w: usize,
        h: usize,
        gray: bool,
        restart: Option<usize>,
        out: &mut S,
    ) {
        let tables = if gray { 1 } else { 2 };
        out.push_bytes(&[0xFF, 0xD8]);
        // APP0 JFIF 1.01, no density, no thumbnail.
        out.push_bytes(&[
            0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0,
        ]);
        segment(out, 0xDB, 65 * tables);
        for (id, table) in self.quant[..tables].iter().enumerate() {
            out.push_bytes(&[id as u8]);
            let mut zigzag = [0u8; 64];
            for (z, &k) in zigzag.iter_mut().zip(&ZIGZAG) {
                *z = table[usize::from(k)] as u8;
            }
            out.push_bytes(&zigzag);
        }
        let comps = if gray { 1 } else { 3 };
        segment(out, 0xC0, 6 + 3 * comps);
        out.push_bytes(&[8]);
        out.push_bytes(&(h as u16).to_be_bytes());
        out.push_bytes(&(w as u16).to_be_bytes());
        out.push_bytes(&[comps as u8]);
        if gray {
            out.push_bytes(&[1, 0x11, 0]);
        } else {
            out.push_bytes(&[1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        }
        let specs = &HUFF[..2 * tables];
        segment(
            out,
            0xC4,
            specs.iter().map(|s| 17 + s.vals.len()).sum::<usize>(),
        );
        for (i, spec) in specs.iter().enumerate() {
            // Class (0 DC, 1 AC) and table id.
            out.push_bytes(&[(((i & 1) << 4) | (i >> 1)) as u8]);
            out.push_bytes(&spec.bits);
            out.push_bytes(spec.vals);
        }
        if let Some(mcus) = restart {
            segment(out, 0xDD, 2);
            out.push_bytes(&(mcus as u16).to_be_bytes());
        }
        segment(out, 0xDA, 4 + 2 * comps);
        out.push_bytes(&[comps as u8]);
        if gray {
            out.push_bytes(&[1, 0x00]);
        } else {
            out.push_bytes(&[1, 0x00, 2, 0x11, 3, 0x11]);
        }
        out.push_bytes(&[0, 63, 0]);
    }
}

/// After MCU row `row` of `rows`: with restart markers, byte-align and write the next one (RST0
/// to RST7 in turn), and reset the DC predictors the decoder resets at it.
fn end_row<S: ByteSink>(
    bits: &mut BitWriter<'_, S>,
    preds: &mut [i16; 3],
    row: usize,
    rows: usize,
    restart: bool,
) {
    if restart && row + 1 < rows {
        bits.flush();
        bits.marker(0xD0 + (row % 8) as u8);
        *preds = [0; 3];
    }
}

/// Whether MCU row `row`'s luma rows are whole in the input: no padding, no copy.
fn luma_direct(input: &PlanarInput<'_>, row: usize, pw: usize) -> bool {
    let (w, h) = input.size();
    let mcu = if input.is_gray() { 8 } else { 16 };
    w == pw && (row + 1) * mcu <= h
}

/// Whether MCU row `row`'s chroma rows (I420) are whole in the input.
fn chroma_direct(input: &PlanarInput<'_>, row: usize, pw: usize) -> bool {
    let (w, h) = input.size();
    w == pw && (row + 1) * 8 <= h.div_ceil(2)
}

/// Copies into the strips what the row needs copied: padded luma (or grey), padded I420 chroma,
/// and NV12's chroma split into Cb and Cr. Whole rows are left where they are.
fn stage_row(input: &PlanarInput<'_>, row: usize, pw: usize, strips: &mut [Vec<u8>; 3]) {
    let (w, h) = input.size();
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mcu = if input.is_gray() { 8 } else { 16 };
    let pcw = pw / 2;
    let [ys, cbs, crs] = strips;
    match *input {
        PlanarInput::Gray { y, stride, .. } => {
            if !luma_direct(input, row, pw) {
                copy_rows(y, stride, (w, h), mcu * row, mcu, ys, pw);
            }
        }
        PlanarInput::I420 {
            y,
            u,
            v,
            y_stride,
            c_stride,
            ..
        } => {
            if !luma_direct(input, row, pw) {
                copy_rows(y, y_stride, (w, h), mcu * row, mcu, ys, pw);
            }
            if !chroma_direct(input, row, pw) {
                copy_rows(u, c_stride, (cw, ch), 8 * row, 8, cbs, pcw);
                copy_rows(v, c_stride, (cw, ch), 8 * row, 8, crs, pcw);
            }
        }
        PlanarInput::Nv12 {
            y,
            uv,
            y_stride,
            uv_stride,
            ..
        } => {
            if !luma_direct(input, row, pw) {
                copy_rows(y, y_stride, (w, h), mcu * row, mcu, ys, pw);
            }
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
    }
}

/// The samples of MCU row `row`: the input's own rows where they are whole, else the strips.
fn row_samples<'a>(
    input: &PlanarInput<'a>,
    row: usize,
    pw: usize,
    strips: &'a [Vec<u8>; 3],
) -> RowSamples<'a> {
    let mcu = if input.is_gray() { 8 } else { 16 };
    let pcw = pw / 2;
    let luma_from = |src: &'a [u8], stride: usize| {
        if luma_direct(input, row, pw) {
            Samples {
                data: &src[mcu * row * stride..],
                stride,
            }
        } else {
            Samples {
                data: &strips[0],
                stride: pw,
            }
        }
    };
    match *input {
        PlanarInput::Gray { y, stride, .. } => RowSamples {
            luma: luma_from(y, stride),
            chroma: None,
        },
        PlanarInput::I420 {
            y,
            u,
            v,
            y_stride,
            c_stride,
            ..
        } => {
            let chroma = |src: &'a [u8], strip: &'a [u8]| {
                if chroma_direct(input, row, pw) {
                    Samples {
                        data: &src[8 * row * c_stride..],
                        stride: c_stride,
                    }
                } else {
                    Samples {
                        data: strip,
                        stride: pcw,
                    }
                }
            };
            RowSamples {
                luma: luma_from(y, y_stride),
                chroma: Some([chroma(u, &strips[1]), chroma(v, &strips[2])]),
            }
        }
        PlanarInput::Nv12 { y, y_stride, .. } => RowSamples {
            luma: luma_from(y, y_stride),
            chroma: Some([
                Samples {
                    data: &strips[1],
                    stride: pcw,
                },
                Samples {
                    data: &strips[2],
                    stride: pcw,
                },
            ]),
        },
    }
}

/// One MCU row: `luma` holds one band (grey) or two of `bw` blocks each, `chroma` the Cb and
/// Cr bands of `bw / 2` blocks.
fn code_mcu_row<S: ByteSink>(
    bits: &mut BitWriter<'_, S>,
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
fn segment<S: ByteSink>(out: &mut S, marker: u8, len: usize) {
    out.push_bytes(&[0xFF, marker]);
    out.push_bytes(&((len + 2) as u16).to_be_bytes());
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
