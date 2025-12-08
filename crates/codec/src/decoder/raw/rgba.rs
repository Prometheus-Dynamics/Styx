use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use rayon::prelude::*;

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn rgba_row_to_rgb24_neon(src: &[u8], dst: &mut [u8], width: usize) {
    use std::arch::aarch64::{uint8x16x3_t, vld4q_u8, vst3q_u8};
    debug_assert!(src.len() >= width * 4);
    debug_assert!(dst.len() >= width * 3);

    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();

    let mut x = 0usize;
    while x + 16 <= width {
        unsafe {
            let rgba = vld4q_u8(src_ptr.add(x * 4));
            let rgb = uint8x16x3_t(rgba.0, rgba.1, rgba.2);
            vst3q_u8(dst_ptr.add(x * 3), rgb);
        }
        x += 16;
    }
    for x in x..width {
        unsafe {
            let si = x * 4;
            let di = x * 3;
            *dst_ptr.add(di) = *src_ptr.add(si);
            *dst_ptr.add(di + 1) = *src_ptr.add(si + 1);
            *dst_ptr.add(di + 2) = *src_ptr.add(si + 2);
        }
    }
}

/// RGBA → RGB24 decoder (drops alpha).
pub struct RgbaToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
}

impl RgbaToRgbDecoder {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_input(
            BufferPool::with_limits(2, bytes, 4),
            FourCc::new(*b"RGBA"),
            "rgba-strip",
        )
    }

    pub fn with_pool(pool: BufferPool) -> Self {
        Self::with_input(pool, FourCc::new(*b"RGBA"), "rgba-strip")
    }

    pub fn with_input(pool: BufferPool, input: FourCc, impl_name: &'static str) -> Self {
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output: FourCc::new(*b"RG24"),
                name: "rgba2rgb",
                impl_name,
            },
            pool,
        }
    }

    pub fn with_input_for_max(
        input: FourCc,
        impl_name: &'static str,
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self::with_input(BufferPool::with_limits(2, bytes, 4), input, impl_name)
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
            .ok_or_else(|| CodecError::Codec("rgba frame missing plane".into()))?;

        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 4);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("rgba stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("rgba plane buffer too short".into()));
        }

        let row_bytes = width * 3;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("rgba output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("rgba dst buffer too short".into()));
        }

        let src = plane.data();
        dst[..out_len]
            .par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst_line)| {
                let src_line = &src[y * stride..][..width * 4];
                #[cfg(target_arch = "aarch64")]
                unsafe {
                    rgba_row_to_rgb24_neon(src_line, dst_line, width);
                    return;
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    for (dst_px, src_px) in dst_line
                        .chunks_exact_mut(3)
                        .zip(src_line.chunks_exact(4))
                    {
                        dst_px[0] = src_px[0];
                        dst_px[1] = src_px[1];
                        dst_px[2] = src_px[2];
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

impl Codec for RgbaToRgbDecoder {
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

        Ok(unsafe {
            FrameLease::single_plane_uninit(
                meta,
                buf,
                layout.len,
                layout.stride,
            )
        })
    }
}

#[cfg(feature = "image")]
impl ImageDecode for RgbaToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
