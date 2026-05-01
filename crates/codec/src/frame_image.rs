use styx_core::prelude::*;

#[cfg(any(feature = "codec-jpeg-decoder", feature = "raw-decoders"))]
use crate::Codec;
#[cfg(feature = "raw-decoders")]
use crate::decoder::raw::{
    I420ToRgbDecoder, Mono8ToRgbDecoder, Nv12ToLumaDecoder, Nv12ToRgbDecoder, YuyvToLumaDecoder,
    YuyvToRgbDecoder,
};
#[cfg(feature = "codec-jpeg-decoder")]
use crate::mjpeg::MjpegDecoder;

#[cfg(feature = "dynamic-image")]
use crate::decoder::{frame_lease_to_dynamic_image, frame_to_dynamic_image};
#[cfg(feature = "dynamic-image")]
use crate::image_utils::dynamic_image_to_frame;
#[cfg(feature = "dynamic-image")]
use image::DynamicImage;

pub trait FrameLeaseImageExt {
    fn image_bytes_per_pixel(&self) -> Option<usize>;
    fn to_rgba8(self) -> Option<FrameLease>;
    fn to_rgb8(self) -> Option<FrameLease>;
    fn to_luma8(self) -> Option<FrameLease>;
    fn flipv(self) -> FrameLease;
    fn fliph(self) -> FrameLease;
    fn rotate90(self) -> FrameLease;
    fn rotate180(self) -> FrameLease;
    fn rotate270(self) -> FrameLease;
    fn grayscale(self) -> FrameLease;
    fn apply_image_transform(self, transform: FrameTransform) -> FrameLease;
    #[cfg(feature = "dynamic-image")]
    fn to_dynamic_image(&self) -> Option<DynamicImage>;
    #[cfg(feature = "dynamic-image")]
    fn into_dynamic_image(self) -> Result<DynamicImage, Box<FrameLease>>;
    #[cfg(feature = "dynamic-image")]
    fn from_dynamic_image(img: DynamicImage, timestamp: u64) -> Option<FrameLease>;
}

impl FrameLeaseImageExt for FrameLease {
    fn image_bytes_per_pixel(&self) -> Option<usize> {
        packed_bytes_per_pixel(self.meta().format.code)
    }

    fn to_rgba8(self) -> Option<FrameLease> {
        convert_to_rgba(self)
    }

    fn to_rgb8(self) -> Option<FrameLease> {
        convert_to_rgb8(self)
    }

    fn to_luma8(self) -> Option<FrameLease> {
        convert_to_luma8(self)
    }

    fn flipv(self) -> FrameLease {
        self.rotate180().fliph()
    }

    fn fliph(self) -> FrameLease {
        self.apply_image_transform(FrameTransform {
            rotation: Rotation90::Deg0,
            mirror: true,
        })
    }

    fn rotate90(self) -> FrameLease {
        self.apply_image_transform(FrameTransform {
            rotation: Rotation90::Deg90,
            mirror: false,
        })
    }

    fn rotate180(self) -> FrameLease {
        self.apply_image_transform(FrameTransform {
            rotation: Rotation90::Deg180,
            mirror: false,
        })
    }

    fn rotate270(self) -> FrameLease {
        self.apply_image_transform(FrameTransform {
            rotation: Rotation90::Deg270,
            mirror: false,
        })
    }

    fn grayscale(self) -> FrameLease {
        if supports_luma_conversion(self.meta().format.code) {
            return self
                .to_luma8()
                .expect("grayscale conversion should succeed for supported formats");
        }
        self
    }

    fn apply_image_transform(self, transform: FrameTransform) -> FrameLease {
        if transform.is_identity() {
            return self;
        }
        match styx_core::transform::transform_packed_frame(&self, transform) {
            Ok(frame) => frame,
            Err(_) => self,
        }
    }

    #[cfg(feature = "dynamic-image")]
    fn to_dynamic_image(&self) -> Option<DynamicImage> {
        frame_to_dynamic_image(self)
    }

    #[cfg(feature = "dynamic-image")]
    fn into_dynamic_image(self) -> Result<DynamicImage, Box<FrameLease>> {
        match frame_lease_to_dynamic_image(self) {
            Ok(img) => Ok(img),
            Err(frame) => match frame_to_dynamic_image(&frame) {
                Some(img) => Ok(img),
                None => Err(Box::new(frame)),
            },
        }
    }

    #[cfg(feature = "dynamic-image")]
    fn from_dynamic_image(img: DynamicImage, timestamp: u64) -> Option<FrameLease> {
        dynamic_image_to_frame(img, timestamp)
    }
}

fn supports_luma_conversion(code: FourCc) -> bool {
    code.packed_bytes_per_pixel().is_some()
        || matches!(code, FourCc::NV12 | FourCc::YUYV | FourCc::I420)
        || code.is_jpeg_encoded()
}

fn packed_bytes_per_pixel(code: FourCc) -> Option<usize> {
    code.packed_bytes_per_pixel()
}

fn make_single_plane_frame(
    meta: &FrameMeta,
    fourcc: FourCc,
    color: ColorSpace,
    stride: usize,
    raw: Vec<u8>,
) -> Option<FrameLease> {
    let res = meta.format.resolution;
    let len = stride.checked_mul(res.height.get() as usize)?;
    if raw.len() != len {
        return None;
    }
    let pool = BufferPool::with_limits(2, len.max(1), 4);
    let mut buf = pool.lease();
    buf.replace_owned(raw);
    Some(FrameLease::single_plane(
        FrameMeta::new(MediaFormat::new(fourcc, res, color), meta.timestamp),
        buf,
        len,
        stride,
    ))
}

fn convert_to_rgb8(frame: FrameLease) -> Option<FrameLease> {
    let meta = frame.meta().clone();
    let code = meta.format.code;

    if code == FourCc::RG24 {
        return Some(frame);
    }
    #[cfg(feature = "codec-jpeg-decoder")]
    if code.is_jpeg_encoded() {
        let decoder = MjpegDecoder::new_for_input(code, FourCc::RG24);
        return decoder.process(frame).ok();
    }
    #[cfg(feature = "raw-decoders")]
    let res = meta.format.resolution;
    #[cfg(feature = "raw-decoders")]
    let width = res.width.get();
    #[cfg(feature = "raw-decoders")]
    let height = res.height.get();
    #[cfg(feature = "raw-decoders")]
    if code == FourCc::NV12 {
        return Nv12ToRgbDecoder::new(width, height).process(frame).ok();
    }
    #[cfg(feature = "raw-decoders")]
    if code == FourCc::YUYV {
        return YuyvToRgbDecoder::new(width, height).process(frame).ok();
    }
    #[cfg(feature = "raw-decoders")]
    if code == FourCc::I420 {
        return I420ToRgbDecoder::new(width, height).process(frame).ok();
    }
    #[cfg(feature = "raw-decoders")]
    if matches!(code, FourCc::R8 | FourCc::GREY) {
        return Mono8ToRgbDecoder::new(width, height).process(frame).ok();
    }

    convert_packed_to_rgb(frame)
}

fn convert_to_rgba(frame: FrameLease) -> Option<FrameLease> {
    let meta = frame.meta().clone();
    let res = meta.format.resolution;
    let width = res.width.get() as usize;
    let height = res.height.get() as usize;
    let code = meta.format.code;

    if code == FourCc::RGBA {
        return Some(frame);
    }

    let rgb_frame = if code.packed_bytes_per_pixel().is_some()
        || (cfg!(feature = "raw-decoders")
            && matches!(code, FourCc::NV12 | FourCc::YUYV | FourCc::I420))
        || (cfg!(feature = "codec-jpeg-decoder") && code.is_jpeg_encoded())
    {
        frame.to_rgb8()?
    } else {
        return None;
    };

    let plane = rgb_frame.planes().into_iter().next()?;
    let stride = plane.stride().max(width * 3);
    let src = plane.data();
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let src_row = &src[y * stride..][..width * 3];
        let dst_row = &mut out[y * width * 4..(y + 1) * width * 4];
        for (dst_px, src_px) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(3)) {
            dst_px[0] = src_px[0];
            dst_px[1] = src_px[1];
            dst_px[2] = src_px[2];
            dst_px[3] = 255;
        }
    }
    make_single_plane_frame(&meta, FourCc::RGBA, ColorSpace::Srgb, width * 4, out)
}

fn convert_to_luma8(frame: FrameLease) -> Option<FrameLease> {
    let meta = frame.meta().clone();
    let code = meta.format.code;

    if matches!(code, FourCc::R8 | FourCc::GREY) {
        return Some(frame);
    }
    #[cfg(feature = "raw-decoders")]
    let res = meta.format.resolution;
    #[cfg(feature = "raw-decoders")]
    let width = res.width.get();
    #[cfg(feature = "raw-decoders")]
    let height = res.height.get();
    #[cfg(feature = "raw-decoders")]
    if code == FourCc::NV12 {
        return Nv12ToLumaDecoder::new(width, height).process(frame).ok();
    }
    #[cfg(feature = "raw-decoders")]
    if code == FourCc::YUYV {
        return YuyvToLumaDecoder::new(width, height).process(frame).ok();
    }
    #[cfg(feature = "codec-jpeg-decoder")]
    if code.is_jpeg_encoded() {
        let decoder = MjpegDecoder::new_for_input(code, FourCc::RG24);
        let rgb = decoder.process(frame).ok()?;
        return convert_rgb_to_luma(rgb);
    }

    convert_packed_to_luma(frame)
}

fn convert_packed_to_rgb(frame: FrameLease) -> Option<FrameLease> {
    let meta = frame.meta().clone();
    let res = meta.format.resolution;
    let width = res.width.get() as usize;
    let height = res.height.get() as usize;
    let plane = frame.planes().into_iter().next()?;
    match meta.format.code {
        FourCc::RGB3 | FourCc::RG24 => {
            let src = plane.data();
            let stride = plane.stride().max(width * 3);
            let mut out = vec![0u8; width * height * 3];
            for y in 0..height {
                let src_row = &src[y * stride..][..width * 3];
                let dst_row = &mut out[y * width * 3..(y + 1) * width * 3];
                dst_row.copy_from_slice(src_row);
            }
            make_single_plane_frame(&meta, FourCc::RG24, meta.format.color, width * 3, out)
        }
        FourCc::BGR3 | FourCc::BG24 => {
            let src = plane.data();
            let stride = plane.stride().max(width * 3);
            let mut out = vec![0u8; width * height * 3];
            for y in 0..height {
                let src_row = &src[y * stride..][..width * 3];
                let dst_row = &mut out[y * width * 3..(y + 1) * width * 3];
                for (dst_px, src_px) in dst_row.chunks_exact_mut(3).zip(src_row.chunks_exact(3)) {
                    dst_px[0] = src_px[2];
                    dst_px[1] = src_px[1];
                    dst_px[2] = src_px[0];
                }
            }
            make_single_plane_frame(&meta, FourCc::RG24, ColorSpace::Srgb, width * 3, out)
        }
        FourCc::RGBA => {
            let src = plane.data();
            let stride = plane.stride().max(width * 4);
            let mut out = vec![0u8; width * height * 3];
            for y in 0..height {
                let src_row = &src[y * stride..][..width * 4];
                let dst_row = &mut out[y * width * 3..(y + 1) * width * 3];
                for (dst_px, src_px) in dst_row.chunks_exact_mut(3).zip(src_row.chunks_exact(4)) {
                    dst_px.copy_from_slice(&src_px[..3]);
                }
            }
            make_single_plane_frame(&meta, FourCc::RG24, ColorSpace::Srgb, width * 3, out)
        }
        FourCc::BGRA => {
            let src = plane.data();
            let stride = plane.stride().max(width * 4);
            let mut out = vec![0u8; width * height * 3];
            for y in 0..height {
                let src_row = &src[y * stride..][..width * 4];
                let dst_row = &mut out[y * width * 3..(y + 1) * width * 3];
                for (dst_px, src_px) in dst_row.chunks_exact_mut(3).zip(src_row.chunks_exact(4)) {
                    dst_px[0] = src_px[2];
                    dst_px[1] = src_px[1];
                    dst_px[2] = src_px[0];
                }
            }
            make_single_plane_frame(&meta, FourCc::RG24, ColorSpace::Srgb, width * 3, out)
        }
        _ => None,
    }
}

fn convert_rgb_to_luma(frame: FrameLease) -> Option<FrameLease> {
    let meta = frame.meta().clone();
    let res = meta.format.resolution;
    let width = res.width.get() as usize;
    let height = res.height.get() as usize;
    let plane = frame.planes().into_iter().next()?;
    let stride = plane.stride().max(width * 3);
    let src = plane.data();
    let mut out = vec![0u8; width * height];
    for y in 0..height {
        let src_row = &src[y * stride..][..width * 3];
        let dst_row = &mut out[y * width..(y + 1) * width];
        for (dst, src_px) in dst_row.iter_mut().zip(src_row.chunks_exact(3)) {
            let r = src_px[0] as u32;
            let g = src_px[1] as u32;
            let b = src_px[2] as u32;
            *dst = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
        }
    }
    make_single_plane_frame(&meta, FourCc::R8, ColorSpace::Unknown, width, out)
}

fn convert_packed_to_luma(frame: FrameLease) -> Option<FrameLease> {
    let meta = frame.meta().clone();
    let res = meta.format.resolution;
    let width = res.width.get() as usize;
    let height = res.height.get() as usize;
    let plane = frame.planes().into_iter().next()?;
    let src = plane.data();
    match meta.format.code {
        FourCc::RG24 | FourCc::RGB3 => convert_rgb_to_luma(frame),
        FourCc::BGR3 | FourCc::BG24 => {
            let stride = plane.stride().max(width * 3);
            let mut out = vec![0u8; width * height];
            for y in 0..height {
                let src_row = &src[y * stride..][..width * 3];
                let dst_row = &mut out[y * width..(y + 1) * width];
                for (dst, src_px) in dst_row.iter_mut().zip(src_row.chunks_exact(3)) {
                    let r = src_px[2] as u32;
                    let g = src_px[1] as u32;
                    let b = src_px[0] as u32;
                    *dst = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
                }
            }
            make_single_plane_frame(&meta, FourCc::R8, ColorSpace::Unknown, width, out)
        }
        FourCc::RGBA | FourCc::BGRA => {
            let stride = plane.stride().max(width * 4);
            let mut out = vec![0u8; width * height];
            for y in 0..height {
                let src_row = &src[y * stride..][..width * 4];
                let dst_row = &mut out[y * width..(y + 1) * width];
                for (dst, src_px) in dst_row.iter_mut().zip(src_row.chunks_exact(4)) {
                    let (r, g, b) = if meta.format.code == FourCc::RGBA {
                        (src_px[0] as u32, src_px[1] as u32, src_px[2] as u32)
                    } else {
                        (src_px[2] as u32, src_px[1] as u32, src_px[0] as u32)
                    };
                    *dst = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
                }
            }
            make_single_plane_frame(&meta, FourCc::R8, ColorSpace::Unknown, width, out)
        }
        _ => None,
    }
}
