//! MJPEG decoders and raw YUV converters for Styx frames, as native codec implementations.
//!
//! - [`MjpegDecoder`]: one JPEG per frame (`MJPG` or `JPEG`) to RGB24, by libturbojpeg when it
//!   was found at build time (feature `turbojpeg`), otherwise zune-jpeg (pure Rust).
//! - [`RawConverter`]: YUYV, NV12 and I420 to RGB24, and YUYV and NV12 to GREY (luma).
//!
//! [`register`] adds all of them to a [`styx_core::Registry`] as `Backend::Native` candidates.
//! Within a request, turbojpeg is tried before zune-jpeg; a candidate that cannot be built says
//! why in its decision.
//!
//! Every decoder and converter writes into an output lease the caller acquired and allocates
//! nothing per frame of its own. zune-jpeg allocates inside each decode (see the steady-state
//! test for the count).

mod mjpeg;
mod raw;
mod turbo;
pub mod yuv;

pub use mjpeg::MjpegDecoder;
pub use raw::{RawConverter, Source, Target};

use styx_core::{Candidate, CodecDescriptor, Implementation, Registry};
use styx_format::FourCc;

/// Adds every native decoder and converter of this crate to `registry`.
pub fn register<M: 'static>(registry: &mut Registry<M>) {
    for input in [FourCc::MJPG, FourCc::JPEG] {
        registry.register(Candidate::new(
            mjpeg::descriptor(input, "mjpeg-turbojpeg"),
            move || {
                Ok(Implementation::Decoder(Box::new(MjpegDecoder::turbojpeg(
                    input,
                )?)))
            },
        ));
        registry.register(Candidate::new(
            mjpeg::descriptor(input, "mjpeg-zune"),
            move || Ok(Implementation::Decoder(Box::new(MjpegDecoder::zune(input)))),
        ));
    }
    for (source, target) in [
        (Source::Yuyv, Target::Rgb24),
        (Source::Yuyv, Target::Grey),
        (Source::Nv12, Target::Rgb24),
        (Source::Nv12, Target::Grey),
        (Source::I420, Target::Rgb24),
    ] {
        let Some(probe) = RawConverter::new(source, target) else {
            continue;
        };
        let descriptor: CodecDescriptor = probe.descriptor().clone();
        registry.register(Candidate::new(descriptor, move || {
            let converter = RawConverter::new(source, target).ok_or_else(|| {
                styx_core::CodecError::Unavailable("no converter for this pair".into())
            })?;
            Ok(Implementation::Converter(Box::new(converter)))
        }));
    }
}
