//! A baseline JPEG encoder's arithmetic on the GPU: JFIF colour (4:2:0) and the integer DCT with
//! quantisation of `styx_core::simd::jpeg`, bit for bit, one invocation per 8x8 block
//! (`shaders/jpeg.comp`). The quantised blocks come back as a [`JpegGrid`]; Huffman coding is
//! serial and stays on the CPU (`styx_codec::styx_jpeg::StyxJpeg::encode_blocks`).

use styx_core::simd::ColorLayout;

use crate::convert::{ARG_WORDS, GpuConvert, Image, check_rows};
use crate::error::GpuError;

/// SPIR-V of `shaders/jpeg.comp` (`shaders/build.sh` makes it).
pub(crate) const SPIRV: &[u8] = include_bytes!("../shaders/jpeg.spv");

/// Pixels for [`GpuConvert::jpeg_blocks`]; 4:2:0 chroma planes are `width.div_ceil(2)` x
/// `height.div_ceil(2)`.
#[derive(Clone, Copy, Debug)]
pub enum JpegPixels<'a> {
    /// Packed RGB or BGR, 3 or 4 bytes a pixel: JFIF colour, chroma from 2x2 sums.
    Packed {
        image: Image<'a>,
        layout: ColorLayout,
    },
    /// 8-bit grey: one component.
    Gray(Image<'a>),
    /// Planar YUV 4:2:0, taken as JFIF YCbCr.
    I420 {
        y: Image<'a>,
        u: Image<'a>,
        v: Image<'a>,
    },
    /// NV12: luma, then interleaved Cb Cr.
    Nv12 { y: Image<'a>, uv: Image<'a> },
}

/// Where the blocks of an image are: `width` blocks a row, row-major, 64 coefficients a block
/// (natural order). Rows `0..luma_rows` are luma, padded to whole MCUs (16x16 with chroma, 8x8
/// grey). With chroma, each later row holds the Cb blocks of one chroma block row, then its Cr
/// blocks, `width / 2` each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JpegGrid {
    pub width: usize,
    pub luma_rows: usize,
    pub rows: usize,
}

impl JpegGrid {
    pub fn new((w, h): (usize, usize), gray: bool) -> Self {
        let mcu = if gray { 8 } else { 16 };
        let (pw, ph) = (w.next_multiple_of(mcu), h.next_multiple_of(mcu));
        Self {
            width: pw / 8,
            luma_rows: ph / 8,
            rows: ph / 8 + if gray { 0 } else { ph / 16 },
        }
    }

    pub fn blocks(&self) -> usize {
        self.width * self.rows
    }
}

/// A source plane, its row bytes and its rows.
type SourcePlane<'a> = (Image<'a>, usize, usize);

/// `jpeg.comp`'s input kinds (`KIND_*`).
const PACKED: u32 = 0;
const GRAY: u32 = 1;
const I420: u32 = 2;
const NV12: u32 = 3;

impl GpuConvert {
    /// The quantised DCT blocks of `pixels` (`w` x `h`, at most 65535 each) into `out` (resized
    /// to the grid's blocks · 64), with `quant`'s luma and chroma tables (natural order, each
    /// entry at least 1): what the CPU's `fdct_quant_band` gives for the padded planes.
    pub fn jpeg_blocks(
        &mut self,
        pixels: JpegPixels<'_>,
        (w, h): (usize, usize),
        quant: &[[u16; 64]; 2],
        out: &mut Vec<i16>,
    ) -> Result<JpegGrid, GpuError> {
        assert!(
            (1..=65535).contains(&w) && (1..=65535).contains(&h),
            "{w}x{h}"
        );
        assert!(
            quant.iter().flatten().all(|&q| q > 0),
            "a quantiser of zero"
        );
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (kind, bpp, flags, planes): (u32, usize, u32, [Option<SourcePlane<'_>>; 3]) =
            match pixels {
                JpegPixels::Packed { image, layout } => {
                    let bpp = layout.bytes_per_pixel();
                    let bgr = matches!(layout, ColorLayout::Bgr24 | ColorLayout::Bgra32);
                    (
                        PACKED,
                        bpp,
                        u32::from(bgr),
                        [Some((image, w * bpp, h)), None, None],
                    )
                }
                JpegPixels::Gray(image) => (GRAY, 1, 0, [Some((image, w, h)), None, None]),
                JpegPixels::I420 { y, u, v } => (
                    I420,
                    1,
                    0,
                    [Some((y, w, h)), Some((u, cw, ch)), Some((v, cw, ch))],
                ),
                JpegPixels::Nv12 { y, uv } => {
                    (NV12, 1, 0, [Some((y, w, h)), Some((uv, 2 * cw, ch)), None])
                }
            };
        let grid = JpegGrid::new((w, h), kind == GRAY);
        let (mut off, mut stride, mut total) = ([0usize; 3], [0usize; 3], 0);
        for (i, (img, row, rows)) in planes
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((i, (*p)?)))
        {
            check_rows("source", img.data.len(), img.stride, row, rows);
            (off[i], stride[i]) = (total, img.stride);
            total += img.data.len();
        }
        let qoff = total;
        self.grow(qoff + 256, grid.blocks() * 128)?;
        let input = self.input.as_mut().expect("grown");
        for (p, &o) in planes.iter().zip(&off) {
            if let Some((img, ..)) = p {
                input.write(o, img.data);
            }
        }
        let mut q = [0u8; 256];
        for (chunk, &v) in q
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(quant.iter().flatten())
        {
            *chunk = v.to_le_bytes();
        }
        input.write(qoff, &q);
        input.flush()?;
        let words: [u32; ARG_WORDS] = [
            kind,
            grid.width as u32,
            grid.rows as u32,
            stride[0] as u32,
            off[1] as u32,
            stride[1] as u32,
            off[2] as u32,
            stride[2] as u32,
            0,
            bpp as u32,
            flags,
            h as u32,
            w as u32,
            qoff as u32,
            grid.luma_rows as u32,
            0,
            0,
            0,
        ];
        let mut bytes = [0u8; ARG_WORDS * 4];
        for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        let groups = (grid.width.div_ceil(8) as u32, grid.rows.div_ceil(8) as u32);
        self.dispatch(self.jpeg, &bytes, groups)?;
        let output = self.output.as_ref().expect("grown");
        output.invalidate()?;
        out.resize(grid.blocks() * 64, 0);
        let coefs = output.bytes()[..grid.blocks() * 128].as_chunks::<2>().0;
        for (c, b) in out.iter_mut().zip(coefs) {
            *c = i16::from_le_bytes(*b);
        }
        Ok(grid)
    }
}
