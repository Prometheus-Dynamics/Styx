#![doc = include_str!("../README.md")]

use std::any::Any;

use styx_core::prelude::*;

mod policy;
mod registry;
mod stats;

pub use policy::{CodecPolicy, CodecPolicyBuilder, Preference};
pub use registry::{CodecRegistry, CodecRegistryHandle};
pub use stats::CodecStats;

/// Encoders/decoders share the same entry-point; the kind distinguishes behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum CodecKind {
    Encoder,
    Decoder,
}

/// Descriptor for a codec implementation.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct CodecDescriptor {
    pub kind: CodecKind,
    pub input: FourCc,
    pub output: FourCc,
    pub name: &'static str,
    pub impl_name: &'static str,
}

#[derive(Debug, Clone)]
pub struct CodecResidencyCapabilities {
    pub accepted_inputs: &'static [FrameResidency],
    pub possible_outputs: &'static [FrameResidency],
    pub preserves_input_residency: bool,
}

pub(crate) const HOST_OR_DMA_INPUTS: &[FrameResidency] = &[
    FrameResidency::HostOwned,
    FrameResidency::HostExternal,
    FrameResidency::Dmabuf,
];
pub(crate) const PACKET_OR_HOST_INPUTS: &[FrameResidency] = &[
    FrameResidency::CompressedPacket,
    FrameResidency::HostOwned,
    FrameResidency::HostExternal,
    FrameResidency::Dmabuf,
];
pub(crate) const HOST_ONLY_OUTPUTS: &[FrameResidency] = &[FrameResidency::HostOwned];
pub(crate) const HOST_OR_DMA_OUTPUTS: &[FrameResidency] = &[
    FrameResidency::HostOwned,
    FrameResidency::HostExternal,
    FrameResidency::Dmabuf,
];
pub(crate) const COMPRESSED_OUTPUTS: &[FrameResidency] = &[FrameResidency::CompressedPacket];

pub(crate) fn is_compressed_fourcc(code: FourCc) -> bool {
    code == FourCc::new(*b"MJPG")
        || code == FourCc::new(*b"JPEG")
        || code == FourCc::new(*b"H264")
        || code == FourCc::new(*b"H265")
        || code == FourCc::new(*b"HEVC")
}

#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub(crate) fn shared_packet_frame(
    descriptor: &CodecDescriptor,
    meta: &FrameMeta,
    data: &[u8],
    pool: &SharedBufferPool,
) -> Result<FrameLease, CodecError> {
    let mut lease = pool
        .lease()
        .map_err(|err| CodecError::Codec(err.to_string()))?;
    lease
        .try_resize(data.len())
        .map_err(|err| CodecError::Codec(err.to_string()))?;
    lease.as_mut_slice().copy_from_slice(data);
    FrameLease::single_plane_shared(
        FrameMeta::new(
            MediaFormat::new(descriptor.output, meta.format.resolution, meta.format.color),
            meta.timestamp,
        )
        .with_residency(FrameResidency::CompressedPacket),
        lease,
        data.len(),
        data.len(),
    )
    .map_err(|err| CodecError::Codec(err.to_string()))
}

/// Unified codec trait for zero-copy processing.
pub trait Codec: Any + Send + Sync + 'static {
    fn descriptor(&self) -> &CodecDescriptor;

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError>;

    #[cfg(target_os = "linux")]
    fn process_shared(
        &self,
        _input: &FrameLease,
        _pool: &SharedBufferPool,
    ) -> Result<Option<FrameLease>, CodecError> {
        Ok(None)
    }

    fn residency_capabilities(&self) -> CodecResidencyCapabilities {
        let descriptor = self.descriptor();
        match descriptor.kind {
            CodecKind::Decoder => CodecResidencyCapabilities {
                accepted_inputs: if is_compressed_fourcc(descriptor.input) {
                    PACKET_OR_HOST_INPUTS
                } else {
                    HOST_OR_DMA_INPUTS
                },
                possible_outputs: HOST_OR_DMA_OUTPUTS,
                preserves_input_residency: false,
            },
            CodecKind::Encoder => CodecResidencyCapabilities {
                accepted_inputs: HOST_OR_DMA_INPUTS,
                possible_outputs: if is_compressed_fourcc(descriptor.output) {
                    COMPRESSED_OUTPUTS
                } else {
                    HOST_ONLY_OUTPUTS
                },
                preserves_input_residency: false,
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("format mismatch: expected {expected}, got {actual}")]
    FormatMismatch { expected: FourCc, actual: FourCc },
    #[error("codec error: {0}")]
    Codec(String),
    #[error("codec backpressure")]
    Backpressure,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("codec not registered for {0}")]
    NotFound(FourCc),
    #[error(transparent)]
    Codec(#[from] CodecError),
}

pub mod decoder;
pub mod encoder;
#[cfg(feature = "codec-ffmpeg")]
pub mod ffmpeg;
pub mod frame_image;
#[cfg(feature = "dynamic-image")]
pub mod image_any;
#[cfg(feature = "dynamic-image")]
pub mod image_utils;
#[cfg(feature = "codec-mozjpeg")]
pub mod jpeg_encoder;
pub mod mjpeg;
#[cfg(feature = "codec-turbojpeg")]
pub mod mjpeg_turbojpeg;
#[cfg(feature = "codec-zune")]
pub mod mjpeg_zune;
pub mod prelude;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Rg24Passthrough {
        descriptor: CodecDescriptor,
    }

    impl Default for Rg24Passthrough {
        fn default() -> Self {
            Self {
                descriptor: CodecDescriptor {
                    kind: CodecKind::Encoder,
                    input: FourCc::new(*b"RG24"),
                    output: FourCc::new(*b"RG24"),
                    name: "passthrough",
                    impl_name: "test",
                },
            }
        }
    }

    impl Codec for Rg24Passthrough {
        fn descriptor(&self) -> &CodecDescriptor {
            &self.descriptor
        }

        fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
            if input.meta().format.code != self.descriptor.input {
                return Err(CodecError::FormatMismatch {
                    expected: self.descriptor.input,
                    actual: input.meta().format.code,
                });
            }
            Ok(input)
        }
    }

    #[test]
    fn auto_converts_rgba_to_rg24_for_rg24_codecs() {
        let registry = CodecRegistry::with_enabled_codecs_for_max(8, 8).expect("registry");
        registry.register(FourCc::new(*b"RG24"), Arc::new(Rg24Passthrough::default()));
        let handle = registry.handle();

        let res = Resolution::new(2, 2).unwrap();
        let layout = plane_layout_from_dims(res.width, res.height, 4);
        let pool = BufferPool::with_limits(1, layout.len, 4);
        let mut buf = pool.lease();
        buf.resize(layout.len);
        for (i, b) in buf.as_mut_slice().iter_mut().enumerate() {
            *b = i as u8;
        }
        let format = MediaFormat::new(FourCc::new(*b"RGBA"), res, ColorSpace::Srgb);
        let frame =
            FrameLease::single_plane(FrameMeta::new(format, 0), buf, layout.len, layout.stride);

        let out = handle
            .process_named(FourCc::new(*b"RG24"), "test", frame)
            .expect("process");
        assert_eq!(out.meta().format.code, FourCc::new(*b"RG24"));
        assert_eq!(out.meta().format.resolution.width.get(), 2);
        assert_eq!(out.meta().format.resolution.height.get(), 2);

        let data = out.planes().first().unwrap().data();
        assert_eq!(data.len(), 2 * 2 * 3);

        let expected: Vec<u8> = (0u8..16)
            .collect::<Vec<_>>()
            .chunks_exact(4)
            .flat_map(|px| px[..3].iter().copied())
            .collect();
        assert_eq!(data, expected.as_slice());
    }
}
