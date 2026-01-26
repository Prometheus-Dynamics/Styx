//! Decoder namespace with per-format modules.

#[cfg(feature = "image")]
use crate::decoder::raw::yuv_to_rgb;
#[cfg(feature = "image")]
use crate::{Codec, CodecError};
#[cfg(feature = "image")]
use image::{DynamicImage, GenericImageView};
#[cfg(feature = "image")]
use rayon::prelude::*;
#[cfg(feature = "image")]
use std::cell::RefCell;
#[cfg(feature = "image")]
use styx_core::prelude::{
    BufferPool, BufferPoolStats, ColorSpace, FourCc, FrameLease, FrameMeta, MediaFormat, Resolution,
};
#[cfg(feature = "image")]
use yuvutils_rs::{
    YuvBiPlanarImage, YuvConversionMode, YuvPackedImage, YuvPlanarImage, YuvRange,
    YuvStandardMatrix,
};

#[cfg(feature = "codec-ffmpeg")]
pub mod ffmpeg;
pub mod mjpeg;
pub mod raw;

/// Trait to retrieve a `DynamicImage` from any decoder.
#[cfg(feature = "image")]
pub trait ImageDecode {
    fn decode_image(&self, frame: FrameLease) -> Result<DynamicImage, CodecError>;
}

#[cfg(feature = "image")]
pub(crate) fn process_to_dynamic<D: Codec>(
    decoder: &D,
    frame: FrameLease,
) -> Result<DynamicImage, CodecError> {
    // Prefer converting the original input frame directly when possible to avoid unnecessary
    // format conversions (e.g. routing BGRA through an RGB24 decoder) when the goal is a
    // `DynamicImage`.
    match frame_lease_to_dynamic_image(frame) {
        Ok(img) => Ok(img),
        Err(frame) => {
            if let Some(img) = frame_to_dynamic_image(&frame) {
                return Ok(img);
            }
            let decoded = decoder.process(frame)?;
            match frame_lease_to_dynamic_image(decoded) {
                Ok(img) => Ok(img),
                Err(decoded) => frame_to_dynamic_image(&decoded)
                    .ok_or_else(|| CodecError::Codec("unable to convert to DynamicImage".into())),
            }
        }
    }
}

#[cfg(feature = "image")]
thread_local! {
    static PACKED_FRAME_POOLS: RefCell<Vec<(usize, BufferPool)>> = const { RefCell::new(Vec::new()) };
}

#[cfg(feature = "image")]
#[derive(Clone, Debug)]
pub struct PackedFramePoolStats {
    pub min_len: usize,
    pub stats: BufferPoolStats,
}

#[cfg(feature = "image")]
const PACKED_FRAME_POOL_SLOTS: usize = 4;

#[cfg(feature = "image")]
fn packed_frame_pool(len: usize) -> BufferPool {
    PACKED_FRAME_POOLS.with(|pools| {
        let mut pools = pools.borrow_mut();
        if let Some(pos) = pools.iter().position(|(k, _)| *k == len) {
            let (k, pool) = pools.remove(pos);
            pools.insert(0, (k, pool.clone()));
            return pool;
        }

        let pool = BufferPool::with_limits(2, len, 2);
        pools.insert(0, (len, pool.clone()));
        if pools.len() > PACKED_FRAME_POOL_SLOTS {
            pools.truncate(PACKED_FRAME_POOL_SLOTS);
        }
        pool
    })
}

/// Drop per-thread packed frame pools (helps release peak allocations between streams).
#[cfg(feature = "image")]
pub fn clear_packed_frame_pools() {
    PACKED_FRAME_POOLS.with(|pools| pools.borrow_mut().clear());
}

/// Clear packed frame pools for the current thread and all Rayon worker threads.
///
/// The decoder fast-path uses `rayon` for some pixel conversions; since Rayon keeps a global
/// thread pool alive for the lifetime of the process, per-thread buffers can otherwise retain
/// peak allocations even after streams are stopped or codecs are switched.
#[cfg(feature = "image")]
pub fn clear_packed_frame_pools_all_threads() {
    clear_packed_frame_pools();
    rayon::broadcast(|_| clear_packed_frame_pools());
}

#[cfg(feature = "image")]
pub fn packed_frame_pool_stats() -> Vec<PackedFramePoolStats> {
    PACKED_FRAME_POOLS.with(|pools| {
        pools
            .borrow()
            .iter()
            .map(|(len, pool)| PackedFramePoolStats {
                min_len: *len,
                stats: pool.stats(),
            })
            .collect()
    })
}

/// Convert an owned `FrameLease` into a `DynamicImage`.
///
/// This is preferred over [`frame_to_dynamic_image`] when the caller can give up the frame, as it
/// can avoid copies for tightly-packed CPU frames (e.g. `RG24`, `RGBA`, `R8  `).
///
/// This function intentionally only handles packed CPU formats that can be wrapped cheaply.
/// For planar formats (e.g. `NV12`, `YUYV`) and raw sensor formats (e.g. Bayer), prefer routing
/// through the codec registry so the fastest available decoder can be selected.
///
/// If the format is not supported by this fast-path, the original frame is returned as
/// `Err(frame)` so the caller can route it through a decoder.
#[cfg(feature = "image")]
#[allow(clippy::result_large_err)]
pub fn frame_lease_to_dynamic_image(frame: FrameLease) -> Result<DynamicImage, FrameLease> {
    let code = frame.meta().format.code;

    // Only return Ok() for formats we can represent correctly as a `DynamicImage` without
    // performing pixel-format conversions. Everything else should return Err(frame) so callers
    // can route through `frame_to_dynamic_image` (conversion/copy) or a codec.
    let is_packed = code == FourCc::new(*b"R8  ")
        || code == FourCc::new(*b"GREY")
        || code == FourCc::new(*b"NV12")
        || code == FourCc::new(*b"NV21")
        || code == FourCc::new(*b"RG24")
        || code == FourCc::new(*b"RGBA");

    if !is_packed {
        return Err(frame);
    }

    let meta = frame.meta();
    let width = meta.format.resolution.width.get();
    let height = meta.format.resolution.height.get();

    // External-backed frames do not own their underlying CPU buffer; we must copy.
    if frame.is_external() {
        if code == FourCc::new(*b"NV12") || code == FourCc::new(*b"NV21") {
            let planes = frame.planes();
            if planes.is_empty() {
                drop(planes);
                return Err(frame);
            }
            let plane = &planes[0];
            let stride = plane.stride().max(width as usize);
            let expected = width as usize;
            let required = stride.saturating_mul(height as usize);
            if plane.data().len() < required {
                drop(planes);
                return Err(frame);
            }
            let out = if stride == expected {
                let required = expected.saturating_mul(height as usize);
                plane.data()[..required].to_vec()
            } else {
                let required = expected.saturating_mul(height as usize);
                let mut out = vec![0u8; required];
                let dst: *mut u8 = out.as_mut_ptr();
                let src: *const u8 = plane.data().as_ptr();
                for y in 0..height as usize {
                    let src_off = y.saturating_mul(stride);
                    let dst_off = y.saturating_mul(expected);
                    unsafe {
                        std::ptr::copy_nonoverlapping(src.add(src_off), dst.add(dst_off), expected);
                    }
                }
                out
            };
            drop(planes);
            let Some(img) = image::GrayImage::from_raw(width, height, out) else {
                return Err(frame);
            };
            return Ok(DynamicImage::ImageLuma8(img));
        }
        return frame_to_dynamic_image(&frame).ok_or(frame);
    }

    // For NV12/NV21, treat plane0 as luma and ignore UV. If the frame owns its buffers and the
    // luma plane is tightly packed, take the Y plane without copying.
    if code == FourCc::new(*b"NV12") || code == FourCc::new(*b"NV21") {
        let layouts = frame.layouts();
        if let Some(layout) = layouts.first() {
            let expected = width as usize;
            let stride = layout.stride.max(expected);
            let required = stride.saturating_mul(height as usize);
            if layout.offset == 0 && stride == expected {
                let planes = frame.planes();
                if let Some(plane) = planes.first() {
                    if plane.data().len() >= required && layout.len >= required {
                        drop(planes);
                        let (_meta, _layouts, buffers) = frame.into_parts();
                        let mut buf = match buffers.into_iter().next() {
                            Some(buf) => buf,
                            None => {
                                let img = image::GrayImage::new(width, height);
                                return Ok(DynamicImage::ImageLuma8(img));
                            }
                        };
                        buf.truncate(required);
                        let img = image::GrayImage::from_raw(width, height, buf)
                            .unwrap_or_else(|| image::GrayImage::new(width, height));
                        return Ok(DynamicImage::ImageLuma8(img));
                    }
                }
            }
        }
        let planes = frame.planes();
        if planes.is_empty() {
            drop(planes);
            return Err(frame);
        }
        let plane = &planes[0];
        let stride = plane.stride().max(width as usize);
        let expected = width as usize;
        let required = stride.saturating_mul(height as usize);
        if plane.data().len() < required {
            drop(planes);
            return Err(frame);
        }
        let out = if stride == expected {
            let required = expected.saturating_mul(height as usize);
            plane.data()[..required].to_vec()
        } else {
            let required = expected.saturating_mul(height as usize);
            let mut out = vec![0u8; required];
            let dst: *mut u8 = out.as_mut_ptr();
            let src: *const u8 = plane.data().as_ptr();
            for y in 0..height as usize {
                let src_off = y.saturating_mul(stride);
                let dst_off = y.saturating_mul(expected);
                unsafe {
                    std::ptr::copy_nonoverlapping(src.add(src_off), dst.add(dst_off), expected);
                }
            }
            out
        };
        drop(planes);
        let Some(img) = image::GrayImage::from_raw(width, height, out) else {
            return Err(frame);
        };
        return Ok(DynamicImage::ImageLuma8(img));
    }

    let (bytes_per_pixel, wrap) = if code == FourCc::new(*b"R8  ") || code == FourCc::new(*b"GREY")
    {
        (1usize, 0u8)
    } else if code == FourCc::new(*b"RG24") {
        (3usize, 1u8)
    } else if code == FourCc::new(*b"RGBA") {
        (4usize, 2u8)
    } else {
        return Err(frame);
    };
    let expected_stride = (width as usize).saturating_mul(bytes_per_pixel);

    let planes = frame.planes();
    if planes.is_empty() {
        drop(planes);
        return Err(frame);
    }
    let plane_stride = planes[0].stride();
    let plane_len = planes[0].data().len();
    drop(planes);
    let stride = plane_stride.max(expected_stride);
    let required = stride.saturating_mul(height as usize);
    if plane_len < required {
        return Err(frame);
    }

    // Some capture backends allocate slightly larger buffers than strictly required (e.g. padding
    // or alignment), even when the visible image is tightly packed. If stride matches exactly,
    // we can still take/move the buffer by truncating down to the required length.
    let can_take_zero_copy = plane_stride == expected_stride && plane_len >= required;
    if can_take_zero_copy {
        let (_meta, layouts, mut buffers) = frame.into_parts();
        let layout = *layouts
            .first()
            .expect("frame has at least one plane layout");
        let buf = buffers
            .pop()
            .expect("non-external packed frame has an owned buffer");

        let stride = layout.stride.max(expected_stride);
        let required = stride.saturating_mul(height as usize);
        if layout.offset == 0
            && stride == expected_stride
            && layout.len >= required
            && buf.len() >= required
        {
            let mut buf = buf;
            buf.truncate(required);
            match wrap {
                0 => {
                    let img =
                        image::GrayImage::from_raw(width, height, buf).expect("length validated");
                    return Ok(DynamicImage::ImageLuma8(img));
                }
                1 => {
                    let img =
                        image::RgbImage::from_raw(width, height, buf).expect("length validated");
                    return Ok(DynamicImage::ImageRgb8(img));
                }
                _ => {
                    let img =
                        image::RgbaImage::from_raw(width, height, buf).expect("length validated");
                    return Ok(DynamicImage::ImageRgba8(img));
                }
            }
        }

        let data = buf
            .get(layout.offset..layout.offset.saturating_add(layout.len))
            .unwrap_or(&[]);
        return Ok(copy_packed_to_image(
            code,
            width,
            height,
            expected_stride,
            stride,
            data,
        ));
    }

    let planes = frame.planes();
    let plane = &planes[0];
    Ok(copy_packed_to_image(
        code,
        width,
        height,
        expected_stride,
        stride,
        plane.data(),
    ))
}

#[cfg(feature = "image")]
fn copy_packed_to_image(
    code: FourCc,
    width: u32,
    height: u32,
    expected_stride: usize,
    stride: usize,
    data: &[u8],
) -> DynamicImage {
    fn copy_strided(
        out: &mut Vec<u8>,
        expected_stride: usize,
        stride: usize,
        height: u32,
        data: &[u8],
    ) {
        let height = height as usize;
        let required = expected_stride.saturating_mul(height);
        out.clear();
        out.resize(required, 0);
        let dst = out.as_mut_ptr();
        let src = data.as_ptr();
        for y in 0..height {
            let src_off = y.saturating_mul(stride);
            let dst_off = y.saturating_mul(expected_stride);
            unsafe {
                std::ptr::copy_nonoverlapping(src.add(src_off), dst.add(dst_off), expected_stride);
            }
        }
    }

    match code {
        c if c == FourCc::new(*b"R8  ") || c == FourCc::new(*b"GREY") => {
            let mut out = Vec::new();
            copy_strided(&mut out, expected_stride, stride, height, data);
            DynamicImage::ImageLuma8(
                image::GrayImage::from_raw(width, height, out).expect("length validated"),
            )
        }
        c if c == FourCc::new(*b"RG24") => {
            let mut out = Vec::new();
            copy_strided(&mut out, expected_stride, stride, height, data);
            DynamicImage::ImageRgb8(
                image::RgbImage::from_raw(width, height, out).expect("length validated"),
            )
        }
        c if c == FourCc::new(*b"RGBA") => {
            let mut out = Vec::new();
            copy_strided(&mut out, expected_stride, stride, height, data);
            DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(width, height, out).expect("length validated"),
            )
        }
        _ => unreachable!("copy_packed_to_image only called for supported packed formats"),
    }
}

/// Convert a `DynamicImage` to a packed `RG24` frame for codec input.
#[cfg(feature = "image")]
pub fn dynamic_image_to_rg24_frame(img: DynamicImage, timestamp: u64) -> Option<FrameLease> {
    match img {
        DynamicImage::ImageRgb8(rgb) => {
            let (width, height) = rgb.dimensions();
            let res = Resolution::new(width, height)?;
            let stride = (width as usize) * 3;
            let len = stride.checked_mul(height as usize)?;
            let raw = rgb.into_raw();
            if raw.len() != len {
                return None;
            }
            let pool = packed_frame_pool(len);
            let mut buf = pool.lease();
            buf.replace_owned(raw);
            let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
            Some(FrameLease::single_plane(
                FrameMeta::new(format, timestamp),
                buf,
                len,
                stride,
            ))
        }
        other => {
            let rgb = other.into_rgb8();
            let (width, height) = rgb.dimensions();
            let res = Resolution::new(width, height)?;
            let stride = (width as usize) * 3;
            let len = stride.checked_mul(height as usize)?;
            let pool = packed_frame_pool(len);
            let mut buf = pool.lease();
            buf.resize(len);
            buf.as_mut_slice().copy_from_slice(&rgb);
            let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
            Some(FrameLease::single_plane(
                FrameMeta::new(format, timestamp),
                buf,
                len,
                stride,
            ))
        }
    }
}

/// Convert a `DynamicImage` reference to a packed `RG24` frame for codec input.
///
/// This avoids cloning the full image buffer when the caller already holds the image in an `Arc`.
#[cfg(feature = "image")]
pub fn dynamic_image_ref_to_rg24_frame(img: &DynamicImage, timestamp: u64) -> Option<FrameLease> {
    let (width, height) = img.dimensions();
    let res = Resolution::new(width, height)?;
    let stride = (width as usize) * 3;
    let len = stride.checked_mul(height as usize)?;
    let pool = packed_frame_pool(len);
    let mut buf = pool.lease();
    buf.resize(len);
    if let Some(rgb) = img.as_rgb8() {
        let raw = rgb.as_raw();
        if raw.len() < len {
            return None;
        }
        buf.as_mut_slice().copy_from_slice(&raw[..len]);
    } else {
        let rgb = img.to_rgb8();
        let raw = rgb.as_raw();
        if raw.len() < len {
            return None;
        }
        buf.as_mut_slice().copy_from_slice(&raw[..len]);
    }

    let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
    Some(FrameLease::single_plane(
        FrameMeta::new(format, timestamp),
        buf,
        len,
        stride,
    ))
}

#[cfg(feature = "image")]
pub fn frame_to_dynamic_image(frame: &FrameLease) -> Option<DynamicImage> {
    let meta = frame.meta();
    let res = meta.format.resolution;
    let width = res.width.get();
    let height = res.height.get();
    let color = meta.format.color;
    let code = meta.format.code;
    let planes = frame.planes();

    #[inline(always)]
    fn map_colorspace(color: ColorSpace) -> (YuvRange, YuvStandardMatrix) {
        match color {
            ColorSpace::Bt709 => (YuvRange::Limited, YuvStandardMatrix::Bt709),
            ColorSpace::Bt2020 => (YuvRange::Limited, YuvStandardMatrix::Bt2020),
            // In our metadata `Srgb` is used as a "full-range output" hint for camera YUV.
            ColorSpace::Srgb => (YuvRange::Full, YuvStandardMatrix::Bt601),
            ColorSpace::Unknown => (YuvRange::Limited, YuvStandardMatrix::Bt709),
        }
    }

    fn copy_tightly_packed(src: &[u8], len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        out.copy_from_slice(&src[..len]);
        out
    }

    fn copy_strided_packed_external(
        plane_data: &[u8],
        src_stride: usize,
        dst_stride: usize,
        height: usize,
    ) -> Vec<u8> {
        let required_src = src_stride.saturating_mul(height);

        // For external-backed (DMA) buffers, many small strided reads can be dramatically slower on
        // some SoCs. Prefer staging the full plane (including padding) into a contiguous Vec and
        // then repacking from cacheable memory when the plane is large enough.
        //
        // NOTE: This is intentionally not tied strictly to "heavy padding"; even modest padding can
        // still trigger slow strided DMA reads at full-frame resolutions.
        const STAGE_THRESHOLD_BYTES: usize = 256 * 1024;
        let use_contiguous_stage =
            plane_data.len() >= required_src && required_src >= STAGE_THRESHOLD_BYTES;

        if use_contiguous_stage {
            let mut staged = vec![0u8; required_src];
            staged.copy_from_slice(&plane_data[..required_src]);
            return copy_strided_packed(&staged, src_stride, dst_stride, height);
        }

        copy_strided_packed(plane_data, src_stride, dst_stride, height)
    }

    fn copy_strided_packed(plane_data: &[u8], src_stride: usize, dst_stride: usize, height: usize) -> Vec<u8> {
        let required_dst = dst_stride.saturating_mul(height);
        let mut out: Vec<u8> = vec![0u8; required_dst];
        let dst: *mut u8 = out.as_mut_ptr();
        let src: *const u8 = plane_data.as_ptr();
        for y in 0..height {
            let src_off = y.saturating_mul(src_stride);
            let dst_off = y.saturating_mul(dst_stride);
            unsafe {
                std::ptr::copy_nonoverlapping(src.add(src_off), dst.add(dst_off), dst_stride);
            }
        }
        out
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn bgr_row_to_rgb24_neon(src: &[u8], dst: &mut [u8], width: usize) {
        use std::arch::aarch64::{uint8x16x3_t, vld3q_u8, vst3q_u8};
        debug_assert!(src.len() >= width * 3);
        debug_assert!(dst.len() >= width * 3);

        let src_ptr = src.as_ptr();
        let dst_ptr = dst.as_mut_ptr();
        let mut x = 0usize;
        while x + 16 <= width {
            unsafe {
                let bgr = vld3q_u8(src_ptr.add(x * 3));
                let rgb = uint8x16x3_t(bgr.2, bgr.1, bgr.0);
                vst3q_u8(dst_ptr.add(x * 3), rgb);
            }
            x += 16;
        }
        for x in x..width {
            unsafe {
                let si = x * 3;
                let di = x * 3;
                let b = *src_ptr.add(si);
                let g = *src_ptr.add(si + 1);
                let r = *src_ptr.add(si + 2);
                *dst_ptr.add(di) = r;
                *dst_ptr.add(di + 1) = g;
                *dst_ptr.add(di + 2) = b;
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn bgra_row_to_rgba_neon(src: &[u8], dst: &mut [u8], width: usize) {
        use std::arch::aarch64::{uint8x16x4_t, vld4q_u8, vst4q_u8};
        debug_assert!(src.len() >= width * 4);
        debug_assert!(dst.len() >= width * 4);

        let src_ptr = src.as_ptr();
        let dst_ptr = dst.as_mut_ptr();
        let mut x = 0usize;
        while x + 16 <= width {
            unsafe {
                let bgra = vld4q_u8(src_ptr.add(x * 4));
                let rgba = uint8x16x4_t(bgra.2, bgra.1, bgra.0, bgra.3);
                vst4q_u8(dst_ptr.add(x * 4), rgba);
            }
            x += 16;
        }
        for x in x..width {
            unsafe {
                let si = x * 4;
                let di = x * 4;
                dst_ptr.add(di).write(*src_ptr.add(si + 2));
                dst_ptr.add(di + 1).write(*src_ptr.add(si + 1));
                dst_ptr.add(di + 2).write(*src_ptr.add(si));
                dst_ptr.add(di + 3).write(*src_ptr.add(si + 3));
            }
        }
    }

    #[inline(always)]
    fn convert_strided_bgr_to_rgb(width: usize, height: usize, src: &[u8], src_stride: usize) -> Vec<u8> {
        let dst_stride = width * 3;
        let required = dst_stride.saturating_mul(height);
        let mut out = vec![0u8; required];

        let out_ptr: *mut u8 = out.as_mut_ptr();
        for y in 0..height {
            let src_line = &src[y * src_stride..][..width * 3];
            let dst_line = unsafe {
                std::slice::from_raw_parts_mut(out_ptr.add(y * dst_stride), dst_stride)
            };

            #[cfg(target_arch = "aarch64")]
            unsafe {
                bgr_row_to_rgb24_neon(src_line, dst_line, width);
                continue;
            }

            #[cfg(not(target_arch = "aarch64"))]
            {
                for (dst_px, src_px) in dst_line.chunks_exact_mut(3).zip(src_line.chunks_exact(3)) {
                    dst_px[0] = src_px[2];
                    dst_px[1] = src_px[1];
                    dst_px[2] = src_px[0];
                }
            }
        }
        out
    }

    #[inline(always)]
    fn convert_strided_bgra_to_rgba(width: usize, height: usize, src: &[u8], src_stride: usize) -> Vec<u8> {
        let dst_stride = width * 4;
        let required = dst_stride.saturating_mul(height);
        let mut out = vec![0u8; required];

        let out_ptr: *mut u8 = out.as_mut_ptr();
        for y in 0..height {
            let src_line = &src[y * src_stride..][..width * 4];
            let dst_line = unsafe {
                std::slice::from_raw_parts_mut(out_ptr.add(y * dst_stride), dst_stride)
            };

            #[cfg(target_arch = "aarch64")]
            unsafe {
                bgra_row_to_rgba_neon(src_line, dst_line, width);
                continue;
            }

            #[cfg(not(target_arch = "aarch64"))]
            {
                for (dst_px, src_px) in dst_line.chunks_exact_mut(4).zip(src_line.chunks_exact(4)) {
                    dst_px[0] = src_px[2];
                    dst_px[1] = src_px[1];
                    dst_px[2] = src_px[0];
                    dst_px[3] = src_px[3];
                }
            }
        }
        out
    }

    match code {
	        c if c == FourCc::new(*b"R8  ") || c == FourCc::new(*b"GREY") => {
	            let plane = planes.into_iter().next()?;
	            let stride = plane.stride().max(width as usize);
	            let required = stride.checked_mul(height as usize)?;
	            if plane.data().len() < required {
	                return None;
	            }
	            let expected = (width as usize).saturating_mul(1);
	            if stride == expected {
	                let required = expected.checked_mul(height as usize)?;
	                let out = copy_tightly_packed(plane.data(), required);
	                return image::GrayImage::from_raw(width, height, out).map(DynamicImage::ImageLuma8);
	            }
	            let out = copy_strided_packed_external(plane.data(), stride, expected, height as usize);
	            image::GrayImage::from_raw(width, height, out).map(DynamicImage::ImageLuma8)
	        }
        c if c == FourCc::new(*b"RG24") => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 3);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let expected = (width as usize).saturating_mul(3);
            if stride == expected {
                let required = expected.checked_mul(height as usize)?;
                let out = copy_tightly_packed(plane.data(), required);
                return image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8);
            }
            let out = copy_strided_packed_external(plane.data(), stride, expected, height as usize);
            image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::new(*b"RGBA") => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 4);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let expected = (width as usize).saturating_mul(4);
            if stride == expected {
                let required = expected.checked_mul(height as usize)?;
                let out = copy_tightly_packed(plane.data(), required);
                return image::RgbaImage::from_raw(width, height, out).map(DynamicImage::ImageRgba8);
            }
	            let out = copy_strided_packed_external(plane.data(), stride, expected, height as usize);
	            image::RgbaImage::from_raw(width, height, out).map(DynamicImage::ImageRgba8)
	        }
        c if c == FourCc::new(*b"BGR3") => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 3);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let out = convert_strided_bgr_to_rgb(
                width as usize,
                height as usize,
                &plane.data()[..required],
                stride,
            );
            image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::new(*b"BG24") => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 3);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let out = convert_strided_bgr_to_rgb(
                width as usize,
                height as usize,
                &plane.data()[..required],
                stride,
            );
            image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::new(*b"BGRA") => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 4);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let out = convert_strided_bgra_to_rgba(
                width as usize,
                height as usize,
                &plane.data()[..required],
                stride,
            );
            image::RgbaImage::from_raw(width, height, out).map(DynamicImage::ImageRgba8)
        }
        c if c == FourCc::new(*b"XB24") => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 4);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let dst_stride = width as usize * 3;
            let len = dst_stride.checked_mul(height as usize)?;
            let mut out = vec![0u8; len];
            let src = &plane.data()[..required];
            out.par_chunks_mut(dst_stride)
                .enumerate()
                .for_each(|(y, dst_line)| {
                    let start = y * stride;
                    let src_line = &src[start..start + (width as usize * 4)];
                    for (dst_px, src_px) in dst_line.chunks_exact_mut(3).zip(src_line.chunks_exact(4))
                    {
                        dst_px[0] = src_px[2];
                        dst_px[1] = src_px[1];
                        dst_px[2] = src_px[0];
                    }
                });
            image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::new(*b"XR24") => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max(width as usize * 4);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let dst_stride = width as usize * 3;
            let len = dst_stride.checked_mul(height as usize)?;
            let mut out = vec![0u8; len];
            let src = &plane.data()[..required];
            out.par_chunks_mut(dst_stride)
                .enumerate()
                .for_each(|(y, dst_line)| {
                    let start = y * stride;
                    let src_line = &src[start..start + (width as usize * 4)];
                    for (dst_px, src_px) in dst_line.chunks_exact_mut(3).zip(src_line.chunks_exact(4))
                    {
                        dst_px[0] = src_px[0];
                        dst_px[1] = src_px[1];
                        dst_px[2] = src_px[2];
                    }
                });
            image::RgbImage::from_raw(width, height, out).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::new(*b"YUYV") => {
            let plane = planes.into_iter().next()?;
            let stride = plane.stride().max((width as usize) * 2);
            let required = stride.checked_mul(height as usize)?;
            if plane.data().len() < required {
                return None;
            }
            let dst_stride = (width as usize) * 3;
            let rgb_len = dst_stride.checked_mul(height as usize)?;
            let mut rgb = vec![0u8; rgb_len];

            let packed = YuvPackedImage {
                yuy: &plane.data()[..required],
                yuy_stride: stride as u32,
                width,
                height,
            };
            let (range, matrix) = map_colorspace(color);
            if yuvutils_rs::yuyv422_to_rgb(&packed, &mut rgb, dst_stride as u32, range, matrix)
                .is_err()
            {
                // Scalar fallback (threaded) writing into preallocated output.
                let src = &plane.data()[..required];
                rgb.par_chunks_mut(dst_stride)
                    .enumerate()
                    .for_each(|(y, dst_line)| {
                        let line = &src[y * stride..][..(width as usize) * 2];
                        let pair_count = (width as usize) / 2;
                        for pair in 0..pair_count {
                            let si = pair * 4;
                            let di = pair * 6;
                            let y0 = line[si] as i32;
                            let u = line[si + 1] as i32;
                            let y1 = line[si + 2] as i32;
                            let v = line[si + 3] as i32;
                            let (r0, g0, b0) = yuv_to_rgb(y0, u, v, color);
                            let (r1, g1, b1) = yuv_to_rgb(y1, u, v, color);
                            dst_line[di] = r0;
                            dst_line[di + 1] = g0;
                            dst_line[di + 2] = b0;
                            dst_line[di + 3] = r1;
                            dst_line[di + 4] = g1;
                            dst_line[di + 5] = b1;
                        }
                        if (width as usize) % 2 == 1 && (width as usize) >= 1 {
                            let last_x = (width as usize) - 1;
                            let si = (last_x / 2) * 4;
                            let di = last_x * 3;
                            let yv = line[si] as i32;
                            let u = line[si + 1] as i32;
                            let v = line[si + 3] as i32;
                            let (r, g, b) = yuv_to_rgb(yv, u, v, color);
                            dst_line[di] = r;
                            dst_line[di + 1] = g;
                            dst_line[di + 2] = b;
                        }
                    });
            }
            image::RgbImage::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::new(*b"NV12") || c == FourCc::new(*b"NV21") => {
            if planes.len() < 2 {
                return None;
            }
            let y_plane = &planes[0];
            let uv_plane = &planes[1];
            let y_stride = y_plane.stride().max(width as usize);
            let chroma_width = (width as usize).div_ceil(2);
            let uv_stride = uv_plane.stride().max(chroma_width * 2);
            let chroma_height = (height as usize).div_ceil(2);
            let y_required = y_stride.checked_mul(height as usize)?;
            let uv_required = uv_stride.checked_mul(chroma_height)?;
            if y_plane.data().len() < y_required || uv_plane.data().len() < uv_required {
                return None;
            }

            let dst_stride = (width as usize) * 3;
            let rgb_len = dst_stride.checked_mul(height as usize)?;
            let mut rgb = vec![0u8; rgb_len];

            let bi = YuvBiPlanarImage {
                y_plane: &y_plane.data()[..y_required],
                y_stride: y_stride as u32,
                uv_plane: &uv_plane.data()[..uv_required],
                uv_stride: uv_stride as u32,
                width,
                height,
            };
            let (range, matrix) = map_colorspace(color);
            let mode = YuvConversionMode::Balanced;
            let is_nv12 = code == FourCc::new(*b"NV12");
            let ok = if is_nv12 {
                yuvutils_rs::yuv_nv12_to_rgb(&bi, &mut rgb, dst_stride as u32, range, matrix, mode)
            } else {
                yuvutils_rs::yuv_nv21_to_rgb(&bi, &mut rgb, dst_stride as u32, range, matrix, mode)
            };
            if ok.is_err() {
                // Scalar fallback (threaded) writing into preallocated output.
                let y_data = &y_plane.data()[..y_required];
                let uv_data = &uv_plane.data()[..uv_required];
                let chroma_width = (width as usize).div_ceil(2);
                rgb.par_chunks_mut(dst_stride)
                    .enumerate()
                    .for_each(|(y, dst_line)| {
                        let y_line = &y_data[y * y_stride..][..width as usize];
                        let uv_line = &uv_data[(y / 2) * uv_stride..][..chroma_width * 2];
                        for (x, yv) in y_line.iter().enumerate() {
                            let uv_idx = (x / 2) * 2;
                            let (u, v) = if is_nv12 {
                                (uv_line[uv_idx] as i32, uv_line[uv_idx + 1] as i32)
                            } else {
                                (uv_line[uv_idx + 1] as i32, uv_line[uv_idx] as i32)
                            };
                            let (r, g, b) = yuv_to_rgb(*yv as i32, u, v, color);
                            let di = x * 3;
                            dst_line[di] = r;
                            dst_line[di + 1] = g;
                            dst_line[di + 2] = b;
                        }
                    });
            }
            image::RgbImage::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
        }
        c if c == FourCc::new(*b"I420") => {
            if planes.len() < 3 {
                return None;
            }
            let y_plane = &planes[0];
            let u_plane = &planes[1];
            let v_plane = &planes[2];
            let y_stride = y_plane.stride().max(width as usize);
            let chroma_width = (width as usize).div_ceil(2);
            let chroma_height = (height as usize).div_ceil(2);
            let u_stride = u_plane.stride().max(chroma_width);
            let v_stride = v_plane.stride().max(chroma_width);
            let y_required = y_stride.checked_mul(height as usize)?;
            let u_required = u_stride.checked_mul(chroma_height)?;
            let v_required = v_stride.checked_mul(chroma_height)?;
            if y_plane.data().len() < y_required
                || u_plane.data().len() < u_required
                || v_plane.data().len() < v_required
            {
                return None;
            }
            let dst_stride = (width as usize) * 3;
            let rgb_len = dst_stride.checked_mul(height as usize)?;
            let mut rgb = vec![0u8; rgb_len];

            let planar = YuvPlanarImage {
                y_plane: &y_plane.data()[..y_required],
                y_stride: y_stride as u32,
                u_plane: &u_plane.data()[..u_required],
                u_stride: u_stride as u32,
                v_plane: &v_plane.data()[..v_required],
                v_stride: v_stride as u32,
                width,
                height,
            };
            let (range, matrix) = map_colorspace(color);
            if yuvutils_rs::yuv420_to_rgb(&planar, &mut rgb, dst_stride as u32, range, matrix)
                .is_err()
            {
                // Scalar fallback (threaded) writing into preallocated output.
                let y_data = &y_plane.data()[..y_required];
                let u_data = &u_plane.data()[..u_required];
                let v_data = &v_plane.data()[..v_required];
                let chroma_width = (width as usize).div_ceil(2);
                rgb.par_chunks_mut(dst_stride)
                    .enumerate()
                    .for_each(|(y, dst_line)| {
                        let y_line = &y_data[y * y_stride..][..width as usize];
                        let u_line = &u_data[(y / 2) * u_stride..][..chroma_width];
                        let v_line = &v_data[(y / 2) * v_stride..][..chroma_width];
                        for (x, yv) in y_line.iter().enumerate() {
                            let u = u_line[x / 2] as i32;
                            let v = v_line[x / 2] as i32;
                            let (r, g, b) = yuv_to_rgb(*yv as i32, u, v, color);
                            let di = x * 3;
                            dst_line[di] = r;
                            dst_line[di + 1] = g;
                            dst_line[di + 2] = b;
                        }
                    });
            }
            image::RgbImage::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
        }
        _ => None,
    }
}

#[cfg(all(test, feature = "image"))]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_rgb_buffer() {
        let res = Resolution::new(2, 2).unwrap();
        let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
        let stride = res.width.get() as usize * 3;
        let len = stride * res.height.get() as usize - 1; // intentionally short
        let mut buf = BufferPool::with_limits(1, len, 1).lease();
        buf.resize(len);
        let frame = FrameLease::single_plane(FrameMeta::new(format, 0), buf, len, stride);
        assert!(frame_to_dynamic_image(&frame).is_none());
    }

    struct NoopCodec {
        desc: crate::CodecDescriptor,
    }

    impl NoopCodec {
        fn new(input: FourCc) -> Self {
            Self {
                desc: crate::CodecDescriptor {
                    kind: crate::CodecKind::Decoder,
                    input,
                    output: input,
                    name: "noop",
                    impl_name: "noop",
                },
            }
        }
    }

    impl crate::Codec for NoopCodec {
        fn descriptor(&self) -> &crate::CodecDescriptor {
            &self.desc
        }

        fn process(&self, input: FrameLease) -> Result<FrameLease, crate::CodecError> {
            Ok(input)
        }
    }

    #[test]
    fn process_to_dynamic_prefers_input_conversion() {
        // BGRA can't be zero-copy wrapped as RGBA; we should still be able to convert to an image
        // without forcing an intermediate RG24 decode.
        let res = Resolution::new(2, 1).unwrap();
        let format = MediaFormat::new(FourCc::new(*b"BGRA"), res, ColorSpace::Srgb);
        let stride = res.width.get() as usize * 4;
        let len = stride * res.height.get() as usize;
        let mut buf = BufferPool::with_limits(1, len, 1).lease();
        buf.resize(len);
        // Two pixels: blue then red.
        buf.as_mut_slice().copy_from_slice(&[255, 0, 0, 255, 0, 0, 255, 255]);
        let frame = FrameLease::single_plane(FrameMeta::new(format, 0), buf, len, stride);

        let codec = NoopCodec::new(FourCc::new(*b"BGRA"));
        let out = process_to_dynamic(&codec, frame).unwrap();
        let rgba = out.into_rgba8();
        assert_eq!(rgba.as_raw(), &[0, 0, 255, 255, 255, 0, 0, 255]);
    }
}
