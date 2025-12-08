//! FFmpeg-backed decoders.
#[cfg(feature = "codec-ffmpeg")]
pub use crate::ffmpeg::{FfmpegH264Decoder, FfmpegH265Decoder, FfmpegMjpegDecoder};
