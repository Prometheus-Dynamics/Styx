//! [`GpuStyxJpeg`]: [`StyxJpeg`] with its colour, DCT and quantisation on a Vulkan device
//! (`styx_gpuisp::GpuConvert::jpeg_blocks`, bit-exact with the CPU's), Huffman coding on the
//! CPU. The same bytes as [`StyxJpeg::encode`].

use styx_gpuisp::{GpuContext, GpuConvert, Image, JpegPixels};

use super::{StyxJpeg, StyxJpegInput};
use crate::CodecError;

/// The GPU's conversions, the CPU's coder and the block buffer, kept between images.
pub struct GpuStyxJpeg {
    convert: GpuConvert,
    jpeg: StyxJpeg,
    blocks: Vec<i16>,
}

impl std::fmt::Debug for GpuStyxJpeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuStyxJpeg")
            .field("jpeg", &self.jpeg)
            .finish()
    }
}

fn gpu_err(e: styx_gpuisp::GpuError) -> CodecError {
    CodecError::Codec(format!("gpu jpeg: {e}"))
}

impl GpuStyxJpeg {
    /// An encoder at `quality` (1–100) on `ctx`'s device.
    pub fn new(ctx: &GpuContext, quality: u8) -> Result<Self, CodecError> {
        Ok(Self {
            convert: GpuConvert::new(ctx).map_err(gpu_err)?,
            jpeg: StyxJpeg::new(quality),
            blocks: Vec::new(),
        })
    }

    pub fn quality(&self) -> u8 {
        self.jpeg.quality()
    }

    /// Encode `input` into `out` (cleared first; its capacity reused).
    pub fn encode(
        &mut self,
        input: &StyxJpegInput<'_>,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        input.check()?;
        let image = |data, stride| Image { data, stride };
        let pixels = match *input {
            StyxJpegInput::Packed {
                data,
                stride,
                layout,
                ..
            } => JpegPixels::Packed {
                image: image(data, stride),
                layout,
            },
            StyxJpegInput::Gray { y, stride, .. } => JpegPixels::Gray(image(y, stride)),
            StyxJpegInput::I420 {
                y,
                u,
                v,
                y_stride,
                c_stride,
                ..
            } => JpegPixels::I420 {
                y: image(y, y_stride),
                u: image(u, c_stride),
                v: image(v, c_stride),
            },
            StyxJpegInput::Nv12 {
                y,
                uv,
                y_stride,
                uv_stride,
                ..
            } => JpegPixels::Nv12 {
                y: image(y, y_stride),
                uv: image(uv, uv_stride),
            },
        };
        let size = input.size();
        let gray = matches!(input, StyxJpegInput::Gray { .. });
        self.convert
            .jpeg_blocks(pixels, size, self.jpeg.quant(), &mut self.blocks)
            .map_err(gpu_err)?;
        self.jpeg.encode_blocks(size, gray, &self.blocks, out)
    }
}

#[cfg(test)]
mod tests {
    use styx_core::simd::ColorLayout;
    use styx_gpuisp::{DeviceSelect, GpuContext};

    use super::*;

    /// Every Vulkan device here (none: the test checks nothing).
    fn contexts() -> Vec<GpuContext> {
        styx_gpuisp::devices()
            .iter()
            .filter_map(|d| GpuContext::open(DeviceSelect::Index(d.index)).ok())
            .collect()
    }

    fn bytes(n: usize, seed: u32) -> Vec<u8> {
        let mut s = seed.wrapping_mul(2_654_435_761) | 1;
        (0..n)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                // Half smooth, half noise: long zero runs and large coefficients both.
                if i % 2 == 0 {
                    (i / 7) as u8
                } else {
                    (s >> 24) as u8
                }
            })
            .collect()
    }

    #[test]
    fn gpu_bytes_are_the_cpu_bytes() {
        for ctx in contexts() {
            let mut gpu = GpuStyxJpeg::new(&ctx, 80).unwrap();
            let mut cpu = StyxJpeg::new(80);
            let (mut want, mut got) = (Vec::new(), Vec::new());
            for (w, h) in [(1usize, 1usize), (17, 9), (64, 48), (101, 37)] {
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let rgb = bytes(w * h * 4, (w * h) as u32);
                let (y, u) = (bytes(w * h, 3), bytes(2 * cw * ch, 5));
                let inputs = [
                    StyxJpegInput::Packed {
                        data: &rgb,
                        width: w,
                        height: h,
                        stride: w * 3,
                        layout: ColorLayout::Bgr24,
                    },
                    StyxJpegInput::Packed {
                        data: &rgb,
                        width: w,
                        height: h,
                        stride: w * 4,
                        layout: ColorLayout::Rgba32,
                    },
                    StyxJpegInput::Gray {
                        y: &y,
                        width: w,
                        height: h,
                        stride: w,
                    },
                    StyxJpegInput::I420 {
                        y: &y,
                        u: &u[..cw * ch],
                        v: &u[cw * ch..],
                        width: w,
                        height: h,
                        y_stride: w,
                        c_stride: cw,
                    },
                    StyxJpegInput::Nv12 {
                        y: &y,
                        uv: &u,
                        width: w,
                        height: h,
                        y_stride: w,
                        uv_stride: 2 * cw,
                    },
                ];
                for input in &inputs {
                    cpu.encode(input, &mut want).unwrap();
                    gpu.encode(input, &mut got).unwrap();
                    assert!(got == want, "{w}x{h} {input:?}");
                }
            }
        }
    }
}
