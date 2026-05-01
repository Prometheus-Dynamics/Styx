//! Encoder namespace with per-format modules.

pub mod ffmpeg;
#[cfg(all(feature = "codec-mozjpeg", not(feature = "codec-turbojpeg")))]
pub mod mozjpeg;
