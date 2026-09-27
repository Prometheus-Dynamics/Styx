//! A small safe FFmpeg layer over runtime-loaded libraries.
//!
//! It mirrors the parts of the `ffmpeg-next` API Styx uses (same module paths and method
//! names), but calls FFmpeg through runtime-loaded libraries, so FFmpeg is only mapped into a
//! process once an
//! FFmpeg codec, scaler or stream is created.

#![allow(clippy::missing_safety_doc)]

pub(crate) mod loader;
pub mod sys;

use std::ffi::{CStr, CString, c_int};
use std::fmt;
use std::ptr;

use ffmpeg_sys_next as raw;

/// Load the core FFmpeg libraries (libavutil, libavcodec, libswscale).
pub fn init() -> Result<(), Error> {
    loader::core().map(|_| ()).map_err(Error::Unavailable)
}

pub use error::Error;

pub mod error;
use error::check;

pub mod util {
    pub mod error {
        pub use super::super::error::EAGAIN;
    }

    pub mod format {
        pub mod pixel {
            use ffmpeg_sys_next::AVPixelFormat;

            /// A pixel format (`AVPixelFormat`).
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
            pub struct Pixel(pub AVPixelFormat);

            #[allow(non_upper_case_globals)]
            impl Pixel {
                pub const NONE: Self = Self(AVPixelFormat::AV_PIX_FMT_NONE);
                pub const NV12: Self = Self(AVPixelFormat::AV_PIX_FMT_NV12);
                pub const YUV420P: Self = Self(AVPixelFormat::AV_PIX_FMT_YUV420P);
                pub const YUVJ420P: Self = Self(AVPixelFormat::AV_PIX_FMT_YUVJ420P);
                pub const YUVJ422P: Self = Self(AVPixelFormat::AV_PIX_FMT_YUVJ422P);
                pub const YUYV422: Self = Self(AVPixelFormat::AV_PIX_FMT_YUYV422);
                pub const RGB24: Self = Self(AVPixelFormat::AV_PIX_FMT_RGB24);
                pub const BGR24: Self = Self(AVPixelFormat::AV_PIX_FMT_BGR24);
                pub const RGBA: Self = Self(AVPixelFormat::AV_PIX_FMT_RGBA);
                pub const BGRA: Self = Self(AVPixelFormat::AV_PIX_FMT_BGRA);
                pub const GRAY8: Self = Self(AVPixelFormat::AV_PIX_FMT_GRAY8);
                pub const DRM_PRIME: Self = Self(AVPixelFormat::AV_PIX_FMT_DRM_PRIME);
            }

            impl From<AVPixelFormat> for Pixel {
                fn from(value: AVPixelFormat) -> Self {
                    Self(value)
                }
            }

            impl From<Pixel> for AVPixelFormat {
                fn from(value: Pixel) -> Self {
                    value.0
                }
            }
        }
    }
}

use util::format::pixel::Pixel;

fn cstring(s: &str) -> Result<CString, Error> {
    CString::new(s).map_err(|_| Error::InvalidData)
}

pub mod codec;
pub use codec::{Codec, decoder, encoder};

#[cfg(feature = "codec-ffmpeg-format")]
mod format_input;
pub mod frame;
pub mod packet;
pub mod software;
#[cfg(feature = "codec-ffmpeg-format")]
pub use format_input::{format, media};
