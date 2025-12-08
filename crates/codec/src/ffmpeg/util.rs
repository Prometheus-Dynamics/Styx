use ffmpeg_next::{frame::Video as FfFrame, util::format::pixel::Pixel as PixelFormat};
use smallvec::{SmallVec, smallvec};
use styx_core::prelude::*;

use crate::CodecError;

pub(crate) fn init_ffmpeg() -> Result<(), CodecError> {
    ffmpeg_next::init().map_err(|e| CodecError::Codec(e.to_string()))
}

/// Scaling context wrapper so we can mark it Send (FFmpeg types are raw pointers).
pub(crate) struct SendSyncScalingContext(pub ffmpeg_next::software::scaling::context::Context);
unsafe impl Send for SendSyncScalingContext {}
unsafe impl Sync for SendSyncScalingContext {}

pub(crate) fn pixel_format_for_fourcc(fourcc: FourCc) -> Option<PixelFormat> {
    match fourcc.to_string().as_str() {
        "NV12" => Some(PixelFormat::NV12),
        "I420" => Some(PixelFormat::YUV420P),
        "YUYV" => Some(PixelFormat::YUYV422),
        "RG24" => Some(PixelFormat::RGB24),
        "RGBA" => Some(PixelFormat::RGBA),
        "BGR3" => Some(PixelFormat::BGR24),
        "BGRA" => Some(PixelFormat::BGRA),
        _ => None,
    }
}

pub(crate) fn bytes_per_pixel(fmt: PixelFormat) -> Option<usize> {
    match fmt {
        PixelFormat::RGB24 | PixelFormat::BGR24 => Some(3),
        PixelFormat::RGBA | PixelFormat::BGRA => Some(4),
        PixelFormat::YUYV422 => Some(2),
        _ => None,
    }
}

pub(crate) fn fourcc_for_pixel_format(fmt: PixelFormat) -> Option<FourCc> {
    match fmt {
        PixelFormat::NV12 => Some(FourCc::new(*b"NV12")),
        PixelFormat::YUV420P | PixelFormat::YUVJ420P => Some(FourCc::new(*b"I420")),
        PixelFormat::YUYV422 => Some(FourCc::new(*b"YUYV")),
        PixelFormat::RGB24 => Some(FourCc::new(*b"RG24")),
        PixelFormat::RGBA => Some(FourCc::new(*b"RGBA")),
        PixelFormat::BGR24 => Some(FourCc::new(*b"BGR3")),
        PixelFormat::BGRA => Some(FourCc::new(*b"BGRA")),
        _ => None,
    }
}

pub(crate) fn layouts_for_frame(
    fmt: PixelFormat,
    frame: &FfFrame,
) -> Option<SmallVec<[PlaneLayout; 3]>> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let mut layouts: SmallVec<[PlaneLayout; 3]> = smallvec![];
    match fmt {
        PixelFormat::NV12 => {
            let stride_y = frame.stride(0);
            let stride_uv = frame.stride(1);
            layouts.push(PlaneLayout {
                offset: 0,
                len: stride_y.saturating_mul(height).min(frame.data(0).len()),
                stride: stride_y,
            });
            layouts.push(PlaneLayout {
                offset: 0,
                len: stride_uv
                    .saturating_mul(height / 2)
                    .min(frame.data(1).len()),
                stride: stride_uv,
            });
        }
        PixelFormat::YUV420P | PixelFormat::YUVJ420P => {
            let stride_y = frame.stride(0);
            let stride_u = frame.stride(1);
            let stride_v = frame.stride(2);
            layouts.push(PlaneLayout {
                offset: 0,
                len: stride_y.saturating_mul(height).min(frame.data(0).len()),
                stride: stride_y,
            });
            layouts.push(PlaneLayout {
                offset: 0,
                len: stride_u.saturating_mul(height / 2).min(frame.data(1).len()),
                stride: stride_u,
            });
            layouts.push(PlaneLayout {
                offset: 0,
                len: stride_v.saturating_mul(height / 2).min(frame.data(2).len()),
                stride: stride_v,
            });
        }
        PixelFormat::YUYV422 => {
            let stride = frame.stride(0);
            layouts.push(PlaneLayout {
                offset: 0,
                len: stride.saturating_mul(height).min(frame.data(0).len()),
                stride,
            });
        }
        PixelFormat::RGB24 | PixelFormat::BGR24 => {
            let stride = frame.stride(0);
            layouts.push(PlaneLayout {
                offset: 0,
                len: stride.saturating_mul(height).min(frame.data(0).len()),
                stride,
            });
        }
        PixelFormat::RGBA | PixelFormat::BGRA => {
            let stride = frame.stride(0);
            layouts.push(PlaneLayout {
                offset: 0,
                len: stride.saturating_mul(height).min(frame.data(0).len()),
                stride,
            });
        }
        _ => return None,
    }
    for layout in layouts.iter_mut() {
        if layout.stride == 0 {
            layout.stride = width;
        }
    }
    Some(layouts)
}
