//! Encoder namespace with per-format modules.

pub mod ffmpeg;
#[cfg(feature = "codec-mozjpeg")]
pub mod mozjpeg;
