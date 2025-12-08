use styx_core::prelude::*;

#[cfg(feature = "image")]
use crate::decoder::{ImageDecode, process_to_dynamic};
use crate::{Codec, CodecDescriptor, CodecError, CodecKind};
use rayon::prelude::*;

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn rgb48_row_to_rgb24_neon(src: &[u8], dst: &mut [u8], width: usize, swap_rb: bool) -> bool {
    use std::arch::aarch64::{uint8x8x3_t, vld3q_u16, vshrn_n_u16, vst3_u8};

    // We need to read u16 lanes; require even byte alignment for the row start and stride.
    if (src.as_ptr() as usize) & 0x1 != 0 {
        return false;
    }
    debug_assert!(src.len() >= width * 6);
    debug_assert!(dst.len() >= width * 3);

    let src_u16 = src.as_ptr() as *const u16;
    let dst_ptr = dst.as_mut_ptr();
    let mut x = 0usize;
    while x + 8 <= width {
        unsafe {
            let rgb16 = vld3q_u16(src_u16.add(x * 3));
            let (r16, g16, b16) = if swap_rb {
                (rgb16.2, rgb16.1, rgb16.0)
            } else {
                (rgb16.0, rgb16.1, rgb16.2)
            };
            let r8 = vshrn_n_u16(r16, 8);
            let g8 = vshrn_n_u16(g16, 8);
            let b8 = vshrn_n_u16(b16, 8);
            vst3_u8(dst_ptr.add(x * 3), uint8x8x3_t(r8, g8, b8));
        }
        x += 8;
    }
    for x in x..width {
        unsafe {
            let si = x * 6;
            let r16 = u16::from_le_bytes([*src.get_unchecked(si), *src.get_unchecked(si + 1)]);
            let g16 = u16::from_le_bytes([*src.get_unchecked(si + 2), *src.get_unchecked(si + 3)]);
            let b16 = u16::from_le_bytes([*src.get_unchecked(si + 4), *src.get_unchecked(si + 5)]);
            let (r16, g16, b16) = if swap_rb { (b16, g16, r16) } else { (r16, g16, b16) };
            let di = x * 3;
            *dst_ptr.add(di) = (r16 >> 8) as u8;
            *dst_ptr.add(di + 1) = (g16 >> 8) as u8;
            *dst_ptr.add(di + 2) = (b16 >> 8) as u8;
        }
    }
    true
}

/// 16-bit per channel RGB/BGR → RGB24 (drop precision, optional swap).
pub struct Rgb48ToRgbDecoder {
    descriptor: CodecDescriptor,
    pool: BufferPool,
    swap_rb: bool,
}

impl Rgb48ToRgbDecoder {
    pub fn new(
        input: FourCc,
        impl_name: &'static str,
        swap_rb: bool,
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let bytes = max_width as usize * max_height as usize * 3;
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input,
                output: FourCc::new(*b"RG24"),
                name: "rgb48torgb24",
                impl_name,
            },
            pool: BufferPool::with_limits(2, bytes, 4),
            swap_rb,
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
            .ok_or_else(|| CodecError::Codec("rgb48 frame missing plane".into()))?;
        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        let stride = plane.stride().max(width * 6);
        let required = stride
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("rgb48 stride overflow".into()))?;
        if plane.data().len() < required {
            return Err(CodecError::Codec("rgb48 plane buffer too short".into()));
        }

        let row_bytes = width
            .checked_mul(3)
            .ok_or_else(|| CodecError::Codec("rgb48 output overflow".into()))?;
        let out_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| CodecError::Codec("rgb48 output overflow".into()))?;
        if dst.len() < out_len {
            return Err(CodecError::Codec("rgb48 dst buffer too short".into()));
        }

        let data = plane.data();
        let swap_rb = self.swap_rb;
        dst[..out_len]
            .par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(y, dst_line)| {
                let src_line = &data[y * stride..][..width * 6];
                #[cfg(target_arch = "aarch64")]
                unsafe {
                    if rgb48_row_to_rgb24_neon(src_line, dst_line, width, swap_rb) {
                        return;
                    }
                }
                for (dst_px, src_px) in dst_line
                    .chunks_exact_mut(3)
                    .zip(src_line.chunks_exact(6))
                {
                    let r16 = u16::from_le_bytes([src_px[0], src_px[1]]);
                    let g16 = u16::from_le_bytes([src_px[2], src_px[3]]);
                    let b16 = u16::from_le_bytes([src_px[4], src_px[5]]);
                    let (r16, g16, b16) = if swap_rb {
                        (b16, g16, r16)
                    } else {
                        (r16, g16, b16)
                    };
                    dst_px[0] = (r16 >> 8) as u8;
                    dst_px[1] = (g16 >> 8) as u8;
                    dst_px[2] = (b16 >> 8) as u8;
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

impl Codec for Rgb48ToRgbDecoder {
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
            FrameLease::single_plane_uninit(meta, buf, layout.len, layout.stride)
        })
    }
}

#[cfg(feature = "image")]
impl ImageDecode for Rgb48ToRgbDecoder {
    fn decode_image(&self, frame: FrameLease) -> Result<image::DynamicImage, CodecError> {
        process_to_dynamic(self, frame)
    }
}
