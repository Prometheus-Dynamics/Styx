use image::DynamicImage;
use std::sync::{Mutex, OnceLock};
use styx_core::prelude::*;

use crate::decoder::ImageDecode;
use crate::{Codec, CodecError};

/// Extension helper to produce a DynamicImage directly from a codec output.
pub trait CodecImageExt {
    fn process_image(&self, frame: FrameLease) -> Result<DynamicImage, CodecError>;
    fn decode_image(&self, frame: FrameLease) -> Result<DynamicImage, CodecError> {
        self.process_image(frame)
    }
}

impl<T: Codec + ImageDecode + ?Sized> CodecImageExt for T {
    fn process_image(&self, frame: FrameLease) -> Result<DynamicImage, CodecError> {
        crate::decoder::ImageDecode::decode_image(self, frame)
    }
}

/// Convert an image into a packed `FrameLease` representing the closest matching FourCC.
pub trait IntoFrameLease {
    fn into_frame(self, timestamp: u64) -> Option<FrameLease>;
}

/// Convert an image reference into a packed `FrameLease` (copies pixel data).
pub trait ToFrameLease {
    fn to_frame(&self, timestamp: u64) -> Option<FrameLease>;
}

#[inline]
fn frame_from_raw(
    fourcc: FourCc,
    res: Resolution,
    color: ColorSpace,
    timestamp: u64,
    stride: usize,
    raw: Vec<u8>,
) -> Option<FrameLease> {
    let len = stride.checked_mul(res.height.get() as usize)?;
    if raw.len() != len {
        return None;
    }
    let pool = static_pool(len);
    let mut buf = pool.lease();
    buf.replace_owned(raw);
    Some(FrameLease::single_plane(
        FrameMeta::new(MediaFormat::new(fourcc, res, color), timestamp),
        buf,
        len,
        stride,
    ))
}

#[inline]
fn frame_from_raw_copy(
    fourcc: FourCc,
    res: Resolution,
    color: ColorSpace,
    timestamp: u64,
    stride: usize,
    raw: &[u8],
) -> Option<FrameLease> {
    let len = stride.checked_mul(res.height.get() as usize)?;
    if raw.len() < len {
        return None;
    }
    let pool = static_pool(len);
    let mut buf = pool.lease();
    buf.resize(len);
    buf.as_mut_slice().copy_from_slice(&raw[..len]);
    Some(FrameLease::single_plane(
        FrameMeta::new(MediaFormat::new(fourcc, res, color), timestamp),
        buf,
        len,
        stride,
    ))
}

impl IntoFrameLease for DynamicImage {
    fn into_frame(self, timestamp: u64) -> Option<FrameLease> {
        match self {
            DynamicImage::ImageLuma8(gray) => {
                let (width, height) = gray.dimensions();
                let res = Resolution::new(width, height)?;
                let stride = width as usize;
                frame_from_raw(
                    FourCc::new(*b"R8  "),
                    res,
                    ColorSpace::Unknown,
                    timestamp,
                    stride,
                    gray.into_raw(),
                )
            }
            DynamicImage::ImageRgb8(rgb) => {
                let (width, height) = rgb.dimensions();
                let res = Resolution::new(width, height)?;
                let stride = (width as usize) * 3;
                frame_from_raw(
                    FourCc::new(*b"RG24"),
                    res,
                    ColorSpace::Srgb,
                    timestamp,
                    stride,
                    rgb.into_raw(),
                )
            }
            DynamicImage::ImageRgba8(rgba) => {
                let (width, height) = rgba.dimensions();
                let res = Resolution::new(width, height)?;
                let stride = (width as usize) * 4;
                frame_from_raw(
                    FourCc::new(*b"RGBA"),
                    res,
                    ColorSpace::Srgb,
                    timestamp,
                    stride,
                    rgba.into_raw(),
                )
            }
            other => {
                let rgba = other.into_rgba8();
                DynamicImage::ImageRgba8(rgba).into_frame(timestamp)
            }
        }
    }
}

impl ToFrameLease for DynamicImage {
    fn to_frame(&self, timestamp: u64) -> Option<FrameLease> {
        match self {
            DynamicImage::ImageLuma8(gray) => {
                let (width, height) = gray.dimensions();
                let res = Resolution::new(width, height)?;
                let stride = width as usize;
                frame_from_raw_copy(
                    FourCc::new(*b"R8  "),
                    res,
                    ColorSpace::Unknown,
                    timestamp,
                    stride,
                    gray.as_raw(),
                )
            }
            DynamicImage::ImageRgb8(rgb) => {
                let (width, height) = rgb.dimensions();
                let res = Resolution::new(width, height)?;
                let stride = (width as usize) * 3;
                frame_from_raw_copy(
                    FourCc::new(*b"RG24"),
                    res,
                    ColorSpace::Srgb,
                    timestamp,
                    stride,
                    rgb.as_raw(),
                )
            }
            DynamicImage::ImageRgba8(rgba) => {
                let (width, height) = rgba.dimensions();
                let res = Resolution::new(width, height)?;
                let stride = (width as usize) * 4;
                frame_from_raw_copy(
                    FourCc::new(*b"RGBA"),
                    res,
                    ColorSpace::Srgb,
                    timestamp,
                    stride,
                    rgba.as_raw(),
                )
            }
            other => {
                let rgba = other.to_rgba8();
                DynamicImage::ImageRgba8(rgba).into_frame(timestamp)
            }
        }
    }
}

/// Convert a `DynamicImage` back into the closest packed `FrameLease`.
pub fn dynamic_image_to_frame(img: DynamicImage, timestamp: u64) -> Option<FrameLease> {
    img.into_frame(timestamp)
}

static DYNAMIC_IMAGE_POOL: OnceLock<Mutex<(BufferPool, usize)>> = OnceLock::new();

fn static_pool(chunk: usize) -> BufferPool {
    let lock =
        DYNAMIC_IMAGE_POOL.get_or_init(|| Mutex::new((BufferPool::with_limits(2, chunk, 4), chunk)));
    let mut guard = lock.lock().unwrap();
    if guard.1 < chunk {
        *guard = (BufferPool::with_limits(2, chunk, 4), chunk);
    }
    guard.0.clone()
}

pub fn dynamic_image_pool_stats() -> Option<BufferPoolStats> {
    let lock = DYNAMIC_IMAGE_POOL.get()?;
    let guard = lock.lock().ok()?;
    Some(guard.0.stats())
}

pub fn reset_dynamic_image_pool() {
    if let Some(lock) = DYNAMIC_IMAGE_POOL.get() {
        if let Ok(mut guard) = lock.lock() {
            *guard = (BufferPool::with_limits(0, 1, 0), 1);
        }
    }
}

#[cfg(all(test, feature = "image"))]
mod tests {
    use super::*;

    #[test]
    fn into_frame_preserves_closest_format() {
        let img = DynamicImage::ImageLuma8(image::GrayImage::from_raw(2, 1, vec![1, 2]).unwrap());
        let frame = img.into_frame(123).unwrap();
        assert_eq!(frame.meta().format.code, FourCc::new(*b"R8  "));
        assert_eq!(frame.meta().timestamp, 123);

        let img = DynamicImage::ImageRgb8(image::RgbImage::from_raw(1, 1, vec![3, 4, 5]).unwrap());
        let frame = img.into_frame(7).unwrap();
        assert_eq!(frame.meta().format.code, FourCc::new(*b"RG24"));
        assert_eq!(frame.meta().timestamp, 7);

        let img = DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(1, 1, vec![6, 7, 8, 9]).unwrap(),
        );
        let frame = img.into_frame(0).unwrap();
        assert_eq!(frame.meta().format.code, FourCc::new(*b"RGBA"));
    }
}
