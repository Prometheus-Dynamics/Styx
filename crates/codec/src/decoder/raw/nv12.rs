use styx_core::prelude::*;

use crate::decoder::raw::yuv_to_rgb;
#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use rayon::prelude::*;
use yuvutils_rs::{YuvBiPlanarImage, YuvConversionMode, YuvRange, YuvStandardMatrix};

#[inline(always)]
fn map_colorspace(color: ColorSpace) -> (YuvRange, YuvStandardMatrix) {
    match color {
        ColorSpace::Bt709 => (YuvRange::Limited, YuvStandardMatrix::Bt709),
        ColorSpace::Bt2020 => (YuvRange::Limited, YuvStandardMatrix::Bt2020),
        // When color is marked as `Srgb` in our metadata it usually means "full-range output".
        // libcamera often reports `sYCC` for PiSP outputs; `sYCC` uses a Rec.601 matrix.
        ColorSpace::Srgb => (YuvRange::Full, YuvStandardMatrix::Bt601),
        // Unknown is far more likely to be limited-range BT.709 than full-range BT.601.
        ColorSpace::Unknown => (YuvRange::Limited, YuvStandardMatrix::Bt709),
    }
}

/// CPU NV12 (Y plane + interleaved UV) → RGB24 decoder.
pub struct Nv12ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl Nv12ToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(BufferPool::with_limits(2, bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"NV12"),
                output: FourCc::new(*b"RG24"),
                name: "yuv2rgb",
                impl_name: "nv12-cpu",
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
        let (y_plane_data, uv_plane_data, y_stride, uv_stride) = if planes.len() >= 2 {
            let y_plane = &planes[0];
            let uv_plane = &planes[1];
            let width = meta.format.resolution.width.get() as usize;
            let height = meta.format.resolution.height.get() as usize;
            let chroma_width = width.div_ceil(2);
            let y_stride = y_plane.stride().max(width);
            let uv_stride = uv_plane.stride().max(chroma_width * 2);
            let chroma_height = height.div_ceil(2);

            let y_required = y_stride
                .checked_mul(height)
                .ok_or_else(|| CodecError::Codec("nv12 y stride overflow".into()))?;
            let uv_required = uv_stride
                .checked_mul(chroma_height)
                .ok_or_else(|| CodecError::Codec("nv12 uv stride overflow".into()))?;
            if y_plane.data().len() < y_required || uv_plane.data().len() < uv_required {
                return Err(CodecError::Codec("nv12 plane buffer too short".into()));
            }

            (
                &y_plane.data()[..y_required],
                &uv_plane.data()[..uv_required],
                y_stride,
                uv_stride,
            )
        } else if planes.len() == 1 {
            let plane = &planes[0];
            let width = meta.format.resolution.width.get() as usize;
            let height = meta.format.resolution.height.get() as usize;
            let chroma_width = width.div_ceil(2);
            let stride = plane.stride().max(width);
            let chroma_height = height.div_ceil(2);
            let y_required = stride
                .checked_mul(height)
                .ok_or_else(|| CodecError::Codec("nv12 y stride overflow".into()))?;
            let uv_required = stride
                .checked_mul(chroma_height)
                .ok_or_else(|| CodecError::Codec("nv12 uv stride overflow".into()))?;
            let total_required = y_required
                .checked_add(uv_required)
                .ok_or_else(|| CodecError::Codec("nv12 plane length overflow".into()))?;
            if plane.data().len() < total_required {
                return Err(CodecError::Codec(
                    "nv12 packed plane buffer too short".into(),
                ));
            }
            let uv_min_stride = (chroma_width * 2).max(1);
            let uv_stride = stride.max(uv_min_stride);
            (
                &plane.data()[..y_required],
                &plane.data()[y_required..y_required + uv_required],
                stride,
                uv_stride,
            )
        } else {
            return Err(CodecError::Codec("nv12 frame missing planes".into()));
        };

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let chroma_width = width.div_ceil(2);
        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("nv12 output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("nv12 output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("nv12 dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];

        let color = meta.format.color;
        let (range, matrix) = map_colorspace(color);
        let bi = YuvBiPlanarImage {
            y_plane: y_plane_data,
            y_stride: y_stride as u32,
            uv_plane: uv_plane_data,
            uv_stride: uv_stride as u32,
            width: width as u32,
            height: height as u32,
        };
        if yuvutils_rs::yuv_nv12_to_rgb(
            &bi,
            dst,
            row_bytes as u32,
            range,
            matrix,
            YuvConversionMode::Balanced,
        )
        .is_err()
        {
            dst.par_chunks_mut(row_bytes)
                .enumerate()
                .for_each(|(y, dst_line)| {
                    let y_line = &y_plane_data[y * y_stride..][..width];
                    let uv_line = &uv_plane_data[(y / 2) * uv_stride..][..chroma_width * 2];
                    let pair_count = width / 2;
                    for pair in 0..pair_count {
                        let y_base = pair * 2;
                        let uv_idx = pair * 2;
                        let di = pair * 6;
                        let y0 = unsafe { *y_line.get_unchecked(y_base) as i32 };
                        let y1 = unsafe { *y_line.get_unchecked(y_base + 1) as i32 };
                        let u = unsafe { *uv_line.get_unchecked(uv_idx) as i32 };
                        let v = unsafe { *uv_line.get_unchecked(uv_idx + 1) as i32 };
                        let (r0, g0, b0) = yuv_to_rgb(y0, u, v, color);
                        let (r1, g1, b1) = yuv_to_rgb(y1, u, v, color);
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
                        let uv_idx = (last_idx / 2) * 2;
                        let di = last_idx * 3;
                        let y_val = unsafe { *y_line.get_unchecked(last_idx) as i32 };
                        let u = unsafe { *uv_line.get_unchecked(uv_idx) as i32 };
                        let v = unsafe { *uv_line.get_unchecked(uv_idx + 1) as i32 };
                        let (r, g, b) = yuv_to_rgb(y_val, u, v, color);
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

impl Codec for Nv12ToRgbDecoder {
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
impl ImageDecode for Nv12ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

/// CPU NV12 (Y plane + interleaved UV) → BGR24 decoder.
pub struct Nv12ToBgrDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl Nv12ToBgrDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(BufferPool::with_limits(2, bytes, 4))
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: FourCc::new(*b"NV12"),
                output: FourCc::new(*b"BG24"),
                name: "yuv2bgr",
                impl_name: "nv12-cpu",
            },
            pool,
        }
    }

    /// Decode into a caller-provided tightly-packed BGR24 buffer.
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
        let (y_plane_data, uv_plane_data, y_stride, uv_stride) = if planes.len() >= 2 {
            let y_plane = &planes[0];
            let uv_plane = &planes[1];
            let width = meta.format.resolution.width.get() as usize;
            let height = meta.format.resolution.height.get() as usize;
            let chroma_width = width.div_ceil(2);
            let y_stride = y_plane.stride().max(width);
            let uv_stride = uv_plane.stride().max(chroma_width * 2);
            let chroma_height = height.div_ceil(2);

            let y_required = y_stride
                .checked_mul(height)
                .ok_or_else(|| CodecError::Codec("nv12 y stride overflow".into()))?;
            let uv_required = uv_stride
                .checked_mul(chroma_height)
                .ok_or_else(|| CodecError::Codec("nv12 uv stride overflow".into()))?;
            if y_plane.data().len() < y_required || uv_plane.data().len() < uv_required {
                return Err(CodecError::Codec("nv12 plane buffer too short".into()));
            }

            (
                &y_plane.data()[..y_required],
                &uv_plane.data()[..uv_required],
                y_stride,
                uv_stride,
            )
        } else if planes.len() == 1 {
            let plane = &planes[0];
            let width = meta.format.resolution.width.get() as usize;
            let height = meta.format.resolution.height.get() as usize;
            let chroma_width = width.div_ceil(2);
            let stride = plane.stride().max(width);
            let chroma_height = height.div_ceil(2);
            let y_required = stride
                .checked_mul(height)
                .ok_or_else(|| CodecError::Codec("nv12 y stride overflow".into()))?;
            let uv_required = stride
                .checked_mul(chroma_height)
                .ok_or_else(|| CodecError::Codec("nv12 uv stride overflow".into()))?;
            let total_required = y_required
                .checked_add(uv_required)
                .ok_or_else(|| CodecError::Codec("nv12 plane length overflow".into()))?;
            if plane.data().len() < total_required {
                return Err(CodecError::Codec(
                    "nv12 packed plane buffer too short".into(),
                ));
            }
            let uv_min_stride = (chroma_width * 2).max(1);
            let uv_stride = stride.max(uv_min_stride);
            (
                &plane.data()[..y_required],
                &plane.data()[y_required..y_required + uv_required],
                stride,
                uv_stride,
            )
        } else {
            return Err(CodecError::Codec("nv12 frame missing planes".into()));
        };

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let chroma_width = width.div_ceil(2);
        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("nv12 output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("nv12 output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("nv12 dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];

        let color = meta.format.color;
        let (range, matrix) = map_colorspace(color);
        let bi = YuvBiPlanarImage {
            y_plane: y_plane_data,
            y_stride: y_stride as u32,
            uv_plane: uv_plane_data,
            uv_stride: uv_stride as u32,
            width: width as u32,
            height: height as u32,
        };
        if yuvutils_rs::yuv_nv12_to_bgr(
            &bi,
            dst,
            row_bytes as u32,
            range,
            matrix,
            YuvConversionMode::Balanced,
        )
        .is_err()
        {
            dst.par_chunks_mut(row_bytes)
                .enumerate()
                .for_each(|(y, dst_line)| {
                    let y_line = &y_plane_data[y * y_stride..][..width];
                    let uv_line = &uv_plane_data[(y / 2) * uv_stride..][..chroma_width * 2];
                    let pair_count = width / 2;
                    for pair in 0..pair_count {
                        let y_base = pair * 2;
                        let uv_idx = pair * 2;
                        let di = pair * 6;
                        let y0 = unsafe { *y_line.get_unchecked(y_base) as i32 };
                        let y1 = unsafe { *y_line.get_unchecked(y_base + 1) as i32 };
                        let u = unsafe { *uv_line.get_unchecked(uv_idx) as i32 };
                        let v = unsafe { *uv_line.get_unchecked(uv_idx + 1) as i32 };
                        let (r0, g0, b0) = yuv_to_rgb(y0, u, v, color);
                        let (r1, g1, b1) = yuv_to_rgb(y1, u, v, color);
                        unsafe {
                            *dst_line.get_unchecked_mut(di) = b0;
                            *dst_line.get_unchecked_mut(di + 1) = g0;
                            *dst_line.get_unchecked_mut(di + 2) = r0;
                            *dst_line.get_unchecked_mut(di + 3) = b1;
                            *dst_line.get_unchecked_mut(di + 4) = g1;
                            *dst_line.get_unchecked_mut(di + 5) = r1;
                        }
                    }
                    if width % 2 == 1 && width >= 1 {
                        let last_idx = width - 1;
                        let uv_idx = (last_idx / 2) * 2;
                        let di = last_idx * 3;
                        let y_val = unsafe { *y_line.get_unchecked(last_idx) as i32 };
                        let u = unsafe { *uv_line.get_unchecked(uv_idx) as i32 };
                        let v = unsafe { *uv_line.get_unchecked(uv_idx + 1) as i32 };
                        let (r, g, b) = yuv_to_rgb(y_val, u, v, color);
                        unsafe {
                            *dst_line.get_unchecked_mut(di) = b;
                            *dst_line.get_unchecked_mut(di + 1) = g;
                            *dst_line.get_unchecked_mut(di + 2) = r;
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

impl Codec for Nv12ToBgrDecoder {
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
