//! FFmpeg as a coverage backend for Styx's codecs.
//!
//! With the `ffmpeg` feature, [`register`] adds the FFmpeg implementations to a
//! [`styx_codec_api::Registry`] as [`styx_codec_api::Backend::Ffmpeg`] candidates. Nothing links FFmpeg: the
//! libraries are opened at run time, the first time a candidate is built. A machine without them
//! reports `FFmpeg not present: ...`, and the registry skips those candidates with that reason.
//!
//! Implemented, and proven through FFmpeg's own MJPEG codec:
//!
//! - decoder: `MJPG` to `I420` (FFmpeg's `yuvj420p` is the same bytes; JPEG's full range is not
//!   carried by the Styx format and is not applied);
//! - encoder: `I420` to `MJPG`, opened at the first frame's size.
//!
//! Frames are copied at the FFmpeg boundary (counted in [`COPY_TO_FFMPEG`] and
//! [`COPY_FROM_FFMPEG`]). DRM PRIME frames from hardware decoders are not handled yet: they need
//! FFmpeg-owned dma-bufs per frame, which `styx-frame`'s pool (buffers given once) does not model.
//!
//! The FFmpeg ABI is hand-written for the fields used (see `sys`), against FFmpeg 8 (libavutil 60,
//! libavcodec 62). Only Linux has the loader; elsewhere the backend reports itself unavailable.

#[cfg(all(feature = "ffmpeg", target_os = "linux"))]
mod api;
#[cfg(all(feature = "ffmpeg", target_os = "linux"))]
mod codec;
#[cfg(all(feature = "ffmpeg", target_os = "linux"))]
mod sys;

#[cfg(all(feature = "ffmpeg", target_os = "linux"))]
mod backend {
    use std::path::PathBuf;

    use styx_codec_api::{
        Candidate, CodecDescriptor, CodecError, CodecKind, Implementation, Registry,
    };
    use styx_format::FourCc;

    use crate::api;
    use crate::codec::{FfmpegDecoder, FfmpegEncoder, descriptor};

    pub use crate::codec::{COPY_FROM_FFMPEG, COPY_TO_FFMPEG};

    /// The FFmpeg candidates this backend offers: name, kind, input, output.
    const CANDIDATES: [(&str, CodecKind, FourCc, FourCc); 2] = [
        ("mjpeg", CodecKind::Decoder, FourCc::MJPG, FourCc::I420),
        ("mjpeg", CodecKind::Encoder, FourCc::I420, FourCc::MJPG),
    ];

    /// The (libavutil, libavcodec) majors the libraries on the system search path report, from
    /// their own version functions. Only supported pairs are returned ([`SUPPORTED`]).
    pub fn loaded_versions() -> Result<(u32, u32), String> {
        let api = api::load(&[])?;
        Ok((api.util_major, api.codec_major))
    }

    /// Whether FFmpeg's libraries are present on the system search path, or why not.
    pub fn available() -> Result<(), String> {
        api::load(&[]).map(|_| ())
    }

    /// Like [`available`], searching `dirs` instead of the system path (an empty list searches
    /// the system path).
    pub fn available_in(dirs: &[PathBuf]) -> Result<(), String> {
        api::load(dirs).map(|_| ())
    }

    /// Adds the FFmpeg candidates to `registry`, found on the system search path.
    pub fn register<M: 'static>(registry: &mut Registry<M>) {
        register_in(registry, &[]);
    }

    /// Adds the FFmpeg candidates to `registry`. The libraries are searched in `dirs` (an empty
    /// list searches the system path); a candidate whose libraries are missing says so from its
    /// constructor and is skipped.
    pub fn register_in<M: 'static>(registry: &mut Registry<M>, dirs: &[PathBuf]) {
        for (name, kind, input, output) in CANDIDATES {
            let descriptor: CodecDescriptor = descriptor(kind, input, output, name);
            let dirs = dirs.to_vec();
            let built = descriptor.clone();
            registry.register(Candidate::new(descriptor, move || {
                let api = api::load(&dirs).map_err(CodecError::Unavailable)?;
                Ok(match built.kind {
                    CodecKind::Decoder => {
                        Implementation::Decoder(Box::new(FfmpegDecoder::open(api, built.clone())?))
                    }
                    CodecKind::Encoder => {
                        Implementation::Encoder(Box::new(FfmpegEncoder::new(api, built.clone())))
                    }
                    CodecKind::Converter => {
                        return Err(CodecError::Unavailable(
                            "FFmpeg converters are not implemented".into(),
                        ));
                    }
                })
            }));
        }
    }

    /// The copy counters of this backend, for tests that assert on copies.
    pub fn copy_counters() -> [&'static styx_frame::CopyCounter; 2] {
        [&COPY_TO_FFMPEG, &COPY_FROM_FFMPEG]
    }
}

#[cfg(all(feature = "ffmpeg", target_os = "linux"))]
#[cfg(all(feature = "ffmpeg", target_os = "linux"))]
pub use api::SUPPORTED;
#[cfg(all(feature = "ffmpeg", target_os = "linux"))]
pub use backend::{
    COPY_FROM_FFMPEG, COPY_TO_FFMPEG, available, available_in, copy_counters, loaded_versions,
    register, register_in,
};
