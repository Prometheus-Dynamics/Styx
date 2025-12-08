use styx_core::prelude::*;

use crate::decoder::raw::yuv_to_rgb;
#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use rayon::prelude::*;
use yuvutils_rs::{YuvPlanarImage, YuvRange, YuvStandardMatrix};

#[inline(always)]
fn map_colorspace(color: ColorSpace) -> (YuvRange, YuvStandardMatrix) {
    match color {
        ColorSpace::Bt709 => (YuvRange::Limited, YuvStandardMatrix::Bt709),
        ColorSpace::Bt2020 => (YuvRange::Limited, YuvStandardMatrix::Bt2020),
        ColorSpace::Srgb => (YuvRange::Full, YuvStandardMatrix::Bt601),
        ColorSpace::Unknown => (YuvRange::Limited, YuvStandardMatrix::Bt709),
    }
}

/// CPU I420 (YUV420 planar) → RGB24 decoder.
pub struct I420ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl I420ToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(BufferPool::with_limits(2, bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"I420"),
                output: FourCc::new(*b"RG24"),
                name: "yuv2rgb",
                impl_name: "i420-cpu",
            },
            pool,
        }
    }

    /// Decode into a caller-provided tightly-packed RGB24 buffer.
    ///
    /// `dst` must be at least `width * height * 3` bytes.
    pub fn decode_into(&self, input: &FrameLease, dst: &mut [u8]) -> Result<FrameMeta, CodecError> {
        let meta = input.meta();
        if meta.format.code != self.descriptor.input {
            return Err(CodecError::FormatMismatch {
                expected: self.descriptor.input,
                actual: meta.format.code,
            });
        }
        let planes = input.planes();
        if planes.len() < 3 {
            return Err(CodecError::Codec("i420 frame missing planes".into()));
        }
        let y_plane = &planes[0];
        let u_plane = &planes[1];
        let v_plane = &planes[2];

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let color = meta.format.color;
        let y_stride = y_plane.stride().max(width);
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let u_stride = u_plane.stride().max(chroma_width);
        let v_stride = v_plane.stride().max(chroma_width);

        let y_required = y_stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("i420 y stride overflow".into()))?;
        let u_required = u_stride
            .checked_mul(chroma_height)
            .ok_or_else(|| CodecError::Codec("i420 u stride overflow".into()))?;
        let v_required = v_stride
            .checked_mul(chroma_height)
            .ok_or_else(|| CodecError::Codec("i420 v stride overflow".into()))?;
        if y_plane.data().len() < y_required
            || u_plane.data().len() < u_required
            || v_plane.data().len() < v_required
        {
            return Err(CodecError::Codec("i420 plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("i420 output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("i420 output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("i420 dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];

        let y_data = &y_plane.data()[..y_required];
        let u_data = &u_plane.data()[..u_required];
        let v_data = &v_plane.data()[..v_required];
        let planar = YuvPlanarImage {
            y_plane: y_data,
            y_stride: y_stride as u32,
            u_plane: u_data,
            u_stride: u_stride as u32,
            v_plane: v_data,
            v_stride: v_stride as u32,
            width: width as u32,
            height: height as u32,
        };
        let (range, matrix) = map_colorspace(color);
        if yuvutils_rs::yuv420_to_rgb(&planar, dst, row_bytes as u32, range, matrix).is_err() {
            dst.par_chunks_mut(row_bytes)
                .enumerate()
                .for_each(|(y, dst_line)| {
                    let y_line = &y_plane.data()[y * y_stride..][..width];
                    let u_line = &u_plane.data()[(y / 2) * u_stride..][..chroma_width];
                    let v_line = &v_plane.data()[(y / 2) * v_stride..][..chroma_width];

                    let pair_count = width / 2;
                    for pair in 0..pair_count {
                        let y_base = pair * 2;
                        let di = pair * 6;
                        let y0 = unsafe { *y_line.get_unchecked(y_base) as i32 };
                        let y1 = unsafe { *y_line.get_unchecked(y_base + 1) as i32 };
                        let u_val = unsafe { *u_line.get_unchecked(pair) as i32 };
                        let v_val = unsafe { *v_line.get_unchecked(pair) as i32 };
                        let (r0, g0, b0) = yuv_to_rgb(y0, u_val, v_val, color);
                        let (r1, g1, b1) = yuv_to_rgb(y1, u_val, v_val, color);
                        unsafe {
                            *dst_line.get_unchecked_mut(di) = r0;
                            *dst_line.get_unchecked_mut(di + 1) = g0;
                            *dst_line.get_unchecked_mut(di + 2) = b0;
                            *dst_line.get_unchecked_mut(di + 3) = r1;
                            *dst_line.get_unchecked_mut(di + 4) = g1;
                            *dst_line.get_unchecked_mut(di + 5) = b1;
                        }
                    }
                    if width % 2 == 1 && width >= 1 {
                        let last_idx = width - 1;
                        let chroma_idx = last_idx / 2;
                        let di = last_idx * 3;
                        let y_val = unsafe { *y_line.get_unchecked(last_idx) as i32 };
                        let u_val = unsafe { *u_line.get_unchecked(chroma_idx) as i32 };
                        let v_val = unsafe { *v_line.get_unchecked(chroma_idx) as i32 };
                        let (r, g, b) = yuv_to_rgb(y_val, u_val, v_val, color);
                        unsafe {
                            *dst_line.get_unchecked_mut(di) = r;
                            *dst_line.get_unchecked_mut(di + 1) = g;
                            *dst_line.get_unchecked_mut(di + 2) = b;
                        }
                    }
                });
        }

        Ok(FrameMeta::new(
            MediaFormat::new(
                self.descriptor.output,
                meta.format.resolution,
                meta.format.color,
            ),
            meta.timestamp,
        ))
    }
}

impl Codec for I420ToRgbDecoder {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let layout = plane_layout_from_dims(
            input.meta().format.resolution.width,
            input.meta().format.resolution.height,
            3,
        );
        let mut buf = self.pool.lease();
        unsafe { buf.resize_uninit(layout.len) };
        let meta = self.decode_into(&input, buf.as_mut_slice())?;
        Ok(unsafe { FrameLease::single_plane_uninit(meta, buf, layout.len, layout.stride) })
    }
}

#[cfg(feature = "image")]
impl ImageDecode for I420ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
