use styx_core::prelude::*;

use crate::decoder::raw::yuv_to_rgb;
#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use rayon::prelude::*;
use yuvutils_rs::{
    YuvBiPlanarImage, YuvConversionMode, YuvPackedImage, YuvPlanarImage, YuvRange,
    YuvStandardMatrix,
};

#[inline(always)]
fn map_colorspace(color: ColorSpace) -> (YuvRange, YuvStandardMatrix) {
    match color {
        ColorSpace::Bt709 => (YuvRange::Limited, YuvStandardMatrix::Bt709),
        ColorSpace::Bt2020 => (YuvRange::Limited, YuvStandardMatrix::Bt2020),
        ColorSpace::Srgb => (YuvRange::Full, YuvStandardMatrix::Bt601),
        ColorSpace::Unknown => (YuvRange::Limited, YuvStandardMatrix::Bt709),
    }
}

#[derive(Clone, Copy)]
enum UvOrder {
    Uv,
    Vu,
}

/// NVXX (Y plane + interleaved chroma) → RGB24 decoder.
pub struct NvToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    uv_order: UvOrder,
    subsample_h: usize,
    subsample_v: usize,
}

impl NvToRgbDecoder {
    pub fn new(
        input: FourCc,
        impl_name: &'static str,
        subsample_h: usize,
        subsample_v: usize,
        uv_is_uv: bool,
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(
            input,
            impl_name,
            subsample_h,
            subsample_v,
            uv_is_uv,
            BufferPool::with_limits(2, bytes, 4),
        )
    }

    pub fn with_pool(
        input: FourCc,
        impl_name: &'static str,
        subsample_h: usize,
        subsample_v: usize,
        uv_is_uv: bool,
        pool: BufferPool,
    ) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output: FourCc::new(*b"RG24"),
                name: "yuv2rgb",
                impl_name,
            },
            pool,
            uv_order: if uv_is_uv { UvOrder::Uv } else { UvOrder::Vu },
            subsample_h: subsample_h.max(1),
            subsample_v: subsample_v.max(1),
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
        if planes.len() < 2 {
            return Err(CodecError::Codec("nv frame missing planes".into()));
        }
        let y_plane = &planes[0];
        let uv_plane = &planes[1];

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let chroma_width = width.div_ceil(self.subsample_h);
        let chroma_height = height.div_ceil(self.subsample_v);
        let y_stride = y_plane.stride().max(width);
        let uv_stride = uv_plane.stride().max(chroma_width * 2);

        let y_required = y_stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("nv y stride overflow".into()))?;
        let uv_required = uv_stride
            .checked_mul(chroma_height)
            .ok_or_else(|| CodecError::Codec("nv uv stride overflow".into()))?;
        if y_plane.data().len() < y_required || uv_plane.data().len() < uv_required {
            return Err(CodecError::Codec("nv plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("nv output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("nv output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("nv dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];

        let (range, matrix) = map_colorspace(meta.format.color);
        let y_plane_data = &y_plane.data()[..y_required];
        let uv_plane_data = &uv_plane.data()[..uv_required];
        let bi = YuvBiPlanarImage {
            y_plane: y_plane_data,
            y_stride: y_stride as u32,
            uv_plane: uv_plane_data,
            uv_stride: uv_stride as u32,
            width: width as u32,
            height: height as u32,
        };
        let mode = YuvConversionMode::Balanced;
        let input_code = self.descriptor.input.to_u32().to_le_bytes();
        let ok = match &input_code {
            b"NV21" => Some(yuvutils_rs::yuv_nv21_to_rgb(
                &bi,
                dst,
                row_bytes as u32,
                range,
                matrix,
                mode,
            )),
            b"NV16" => Some(yuvutils_rs::yuv_nv16_to_rgb(
                &bi,
                dst,
                row_bytes as u32,
                range,
                matrix,
                mode,
            )),
            b"NV61" => Some(yuvutils_rs::yuv_nv61_to_rgb(
                &bi,
                dst,
                row_bytes as u32,
                range,
                matrix,
                mode,
            )),
            b"NV24" => Some(yuvutils_rs::yuv_nv24_to_rgb(
                &bi,
                dst,
                row_bytes as u32,
                range,
                matrix,
                mode,
            )),
            b"NV42" => Some(yuvutils_rs::yuv_nv42_to_rgb(
                &bi,
                dst,
                row_bytes as u32,
                range,
                matrix,
                mode,
            )),
            _ => None,
        };
        if ok.is_some_and(|r| r.is_ok()) {
            return Ok(FrameMeta::new(
                MediaFormat::new(
                    self.descriptor.output,
                    meta.format.resolution,
                    meta.format.color,
                ),
                meta.timestamp,
            ));
        }

        let color = meta.format.color;
        let uv_order = self.uv_order;
        let subsample_h = self.subsample_h;
        let subsample_v = self.subsample_v;
        dst.par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst_line)| {
                let y_line = &y_plane_data[y * y_stride..][..width];
                let uv_line =
                    &uv_plane_data[(y / subsample_v) * uv_stride..][..chroma_width * 2];
                for x in 0..width {
                    let uv_base = (x / subsample_h) * 2;
                    let (u, v) = unsafe {
                        match uv_order {
                            UvOrder::Uv => (
                                *uv_line.get_unchecked(uv_base) as i32,
                                *uv_line.get_unchecked(uv_base + 1) as i32,
                            ),
                            UvOrder::Vu => (
                                *uv_line.get_unchecked(uv_base + 1) as i32,
                                *uv_line.get_unchecked(uv_base) as i32,
                            ),
                        }
                    };
                    let y_val = unsafe { *y_line.get_unchecked(x) as i32 };
                    let (r, g, b) = yuv_to_rgb(y_val, u, v, color);
                    let di = x * 3;
                    unsafe {
                        *dst_line.get_unchecked_mut(di) = r;
                        *dst_line.get_unchecked_mut(di + 1) = g;
                        *dst_line.get_unchecked_mut(di + 2) = b;
                    }
                }
            });

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

impl Codec for NvToRgbDecoder {
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
impl ImageDecode for NvToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

/// Planar YUV → RGB24 decoder (Y + separate U/V planes).
pub struct PlanarYuvToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    u_first: bool,
    subsample_h: usize,
    subsample_v: usize,
}

impl PlanarYuvToRgbDecoder {
    pub fn new(
        input: FourCc,
        impl_name: &'static str,
        subsample_h: usize,
        subsample_v: usize,
        u_first: bool,
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(
            input,
            impl_name,
            subsample_h,
            subsample_v,
            u_first,
            BufferPool::with_limits(2, bytes, 4),
        )
    }

    pub fn with_pool(
        input: FourCc,
        impl_name: &'static str,
        subsample_h: usize,
        subsample_v: usize,
        u_first: bool,
        pool: BufferPool,
    ) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output: FourCc::new(*b"RG24"),
                name: "yuv2rgb",
                impl_name,
            },
            pool,
            u_first,
            subsample_h: subsample_h.max(1),
            subsample_v: subsample_v.max(1),
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
            return Err(CodecError::Codec("planar yuv frame missing planes".into()));
        }

        let y_plane = &planes[0];
        let (u_plane, v_plane) = if self.u_first {
            (&planes[1], &planes[2])
        } else {
            (&planes[2], &planes[1])
        };

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let chroma_width = width.div_ceil(self.subsample_h);
        let chroma_height = height.div_ceil(self.subsample_v);
        let y_stride = y_plane.stride().max(width);
        let u_stride = u_plane.stride().max(chroma_width);
        let v_stride = v_plane.stride().max(chroma_width);

        let y_required = y_stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("planar yuv y stride overflow".into()))?;
        let u_required = u_stride
            .checked_mul(chroma_height)
            .ok_or_else(|| CodecError::Codec("planar yuv u stride overflow".into()))?;
        let v_required = v_stride
            .checked_mul(chroma_height)
            .ok_or_else(|| CodecError::Codec("planar yuv v stride overflow".into()))?;
        if y_plane.data().len() < y_required
            || u_plane.data().len() < u_required
            || v_plane.data().len() < v_required
        {
            return Err(CodecError::Codec(
                "planar yuv plane buffer too short".into(),
            ));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("planar yuv output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("planar yuv output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("planar yuv dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];

        let (range, matrix) = map_colorspace(meta.format.color);
        let y_plane_data = &y_plane.data()[..y_required];
        let u_plane_data = &u_plane.data()[..u_required];
        let v_plane_data = &v_plane.data()[..v_required];
        let planar = YuvPlanarImage {
            y_plane: y_plane_data,
            y_stride: y_stride as u32,
            u_plane: u_plane_data,
            u_stride: u_stride as u32,
            v_plane: v_plane_data,
            v_stride: v_stride as u32,
            width: width as u32,
            height: height as u32,
        };
        let ok = match (self.subsample_h, self.subsample_v) {
            (2, 2) => Some(yuvutils_rs::yuv420_to_rgb(
                &planar,
                dst,
                row_bytes as u32,
                range,
                matrix,
            )),
            (2, 1) => Some(yuvutils_rs::yuv422_to_rgb(
                &planar,
                dst,
                row_bytes as u32,
                range,
                matrix,
            )),
            (1, 1) => Some(yuvutils_rs::yuv444_to_rgb(
                &planar,
                dst,
                row_bytes as u32,
                range,
                matrix,
            )),
            _ => None,
        };
        if ok.is_some_and(|r| r.is_ok()) {
            return Ok(FrameMeta::new(
                MediaFormat::new(
                    self.descriptor.output,
                    meta.format.resolution,
                    meta.format.color,
                ),
                meta.timestamp,
            ));
        }

        let color = meta.format.color;
        let subsample_h = self.subsample_h;
        let subsample_v = self.subsample_v;
        dst.par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst_line)| {
                let y_line = &y_plane_data[y * y_stride..][..width];
                let u_line =
                    &u_plane_data[(y / subsample_v) * u_stride..][..chroma_width];
                let v_line =
                    &v_plane_data[(y / subsample_v) * v_stride..][..chroma_width];
                for x in 0..width {
                    let chroma_idx = x / subsample_h;
                    let u = unsafe { *u_line.get_unchecked(chroma_idx) as i32 };
                    let v = unsafe { *v_line.get_unchecked(chroma_idx) as i32 };
                    let y_val = unsafe { *y_line.get_unchecked(x) as i32 };
                    let (r, g, b) = yuv_to_rgb(y_val, u, v, color);
                    let di = x * 3;
                    unsafe {
                        *dst_line.get_unchecked_mut(di) = r;
                        *dst_line.get_unchecked_mut(di + 1) = g;
                        *dst_line.get_unchecked_mut(di + 2) = b;
                    }
                }
            });

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

impl Codec for PlanarYuvToRgbDecoder {
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
impl ImageDecode for PlanarYuvToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}

/// Packed YUV422 → RGB24 decoder with configurable byte order.
pub struct Packed422ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    byte_order: [usize; 4],
}

impl Packed422ToRgbDecoder {
    pub fn new(
        input: FourCc,
        impl_name: &'static str,
        byte_order: [usize; 4],
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_pool(
            input,
            impl_name,
            byte_order,
            BufferPool::with_limits(2, bytes, 4),
        )
    }

    pub fn with_pool(
        input: FourCc,
        impl_name: &'static str,
        byte_order: [usize; 4],
        pool: BufferPool,
    ) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output: FourCc::new(*b"RG24"),
                name: "yuv2rgb",
                impl_name,
            },
            pool,
            byte_order,
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
        let plane = input
            .planes()
            .into_iter()
            .next()
            .ok_or_else(|| CodecError::Codec("yuv422 frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 2);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("yuv422 stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("yuv422 plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("yuv422 output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("yuv422 output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("yuv422 dst buffer too short".into()));
        }
        let dst = &mut dst[..out_len];

        let src = plane.data();
        let src_required = &src[..required];
        let (range, matrix) = map_colorspace(meta.format.color);
        let packed = YuvPackedImage {
            yuy: src_required,
            yuy_stride: stride as u32,
            width: width as u32,
            height: height as u32,
        };
        let input_code = self.descriptor.input.to_u32().to_le_bytes();
        let ok = match &input_code {
            b"UYVY" => Some(yuvutils_rs::uyvy422_to_rgb(
                &packed,
                dst,
                row_bytes as u32,
                range,
                matrix,
            )),
            b"YUYV" => Some(yuvutils_rs::yuyv422_to_rgb(
                &packed,
                dst,
                row_bytes as u32,
                range,
                matrix,
            )),
            _ => None,
        };
        if ok.is_some_and(|r| r.is_ok()) {
            return Ok(FrameMeta::new(
                MediaFormat::new(
                    self.descriptor.output,
                    meta.format.resolution,
                    meta.format.color,
                ),
                meta.timestamp,
            ));
        }

        let color = meta.format.color;
        let byte_order = self.byte_order;
        dst.par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst_line)| {
                let src_line = &src_required[y * stride..][..width * 2];
                let pair_count = width / 2;
                for pair in 0..pair_count {
                    let si = pair * 4;
                    let di = pair * 6;
                    let y0 = unsafe { *src_line.get_unchecked(si + byte_order[0]) as i32 };
                    let u = unsafe { *src_line.get_unchecked(si + byte_order[1]) as i32 };
                    let y1 = unsafe { *src_line.get_unchecked(si + byte_order[2]) as i32 };
                    let v = unsafe { *src_line.get_unchecked(si + byte_order[3]) as i32 };
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
                    let last_x = width - 1;
                    let si = (last_x / 2) * 4;
                    let di = last_x * 3;
                    let y_val = unsafe { *src_line.get_unchecked(si + byte_order[0]) as i32 };
                    let u = unsafe { *src_line.get_unchecked(si + byte_order[1]) as i32 };
                    let v = unsafe { *src_line.get_unchecked(si + byte_order[3]) as i32 };
                    let (r, g, b) = yuv_to_rgb(y_val, u, v, color);
                    unsafe {
                        *dst_line.get_unchecked_mut(di) = r;
                        *dst_line.get_unchecked_mut(di + 1) = g;
                        *dst_line.get_unchecked_mut(di + 2) = b;
                    }
                }
            });

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

impl Codec for Packed422ToRgbDecoder {
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
impl ImageDecode for Packed422ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
