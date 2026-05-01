#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]

use std::any::Any;

use styx_core::prelude::*;

mod policy;
mod registry;
mod stats;

pub use policy::{CodecPolicy, CodecPolicyBuilder, Preference};
pub use registry::{
    CodecRegistry, CodecRegistryConfig, CodecRegistryHandle, DEFAULT_CODEC_MAX_HEIGHT,
    DEFAULT_CODEC_MAX_WIDTH,
};
pub use stats::CodecStats;

#[allow(dead_code)]
pub(crate) const DEFAULT_CODEC_POOL_CHUNK_BYTES: usize = 64 * 1024;
#[allow(dead_code)]
pub(crate) const DEFAULT_CODEC_POOL_SPARE: usize = 4;

/// Encoders/decoders share the same entry-point; the kind distinguishes behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum CodecKind {
    Encoder,
    Decoder,
}

impl std::fmt::Display for CodecKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Encoder => "encoder",
            Self::Decoder => "decoder",
        })
    }
}

impl std::str::FromStr for CodecKind {
    type Err = CodecKindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "encoder" | "encode" => Ok(Self::Encoder),
            "decoder" | "decode" => Ok(Self::Decoder),
            _ => Err(CodecKindParseError {
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecKindParseError {
    value: String,
}

impl std::fmt::Display for CodecKindParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown codec kind '{}'", self.value)
    }
}

impl std::error::Error for CodecKindParseError {}

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

/// Normalized codec implementation identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = String))]
pub struct CodecImplementationId(std::borrow::Cow<'static, str>);

impl CodecImplementationId {
    pub const FFMPEG: Self = Self(std::borrow::Cow::Borrowed("ffmpeg"));
    pub const JPEG_DECODER: Self = Self(std::borrow::Cow::Borrowed("jpeg-decoder"));
    pub const MOZJPEG: Self = Self(std::borrow::Cow::Borrowed("mozjpeg"));
    pub const PASSTHROUGH: Self = Self(std::borrow::Cow::Borrowed("passthrough"));
    pub const TURBOJPEG: Self = Self(std::borrow::Cow::Borrowed("turbojpeg"));
    pub const ZUNE_JPEG: Self = Self(std::borrow::Cow::Borrowed("zune-jpeg"));

    pub fn new(value: impl AsRef<str>) -> Self {
        Self(std::borrow::Cow::Owned(
            value.as_ref().trim().to_ascii_lowercase(),
        ))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }

    pub fn is_hardware_accelerated(&self) -> bool {
        is_hardware_implementation_name(self.as_str())
    }
}

impl std::fmt::Display for CodecImplementationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for CodecImplementationId {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(value))
    }
}

impl From<&str> for CodecImplementationId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for CodecImplementationId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&String> for CodecImplementationId {
    fn from(value: &String) -> Self {
        Self::new(value)
    }
}

impl AsRef<str> for CodecImplementationId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl CodecDescriptor {
    pub fn implementation_id(&self) -> CodecImplementationId {
        CodecImplementationId::new(self.impl_name)
    }

    pub fn is_hardware_accelerated(&self) -> bool {
        self.implementation_id().is_hardware_accelerated()
            || is_hardware_implementation_name(self.name)
    }
}

pub fn is_hardware_implementation_name(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "vaapi",
        "nvenc",
        "nvdec",
        "cuvid",
        "qsv",
        "v4l2",
        "videotoolbox",
        "v4l2m2m",
    ]
    .iter()
    .any(|token| value.contains(token))
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

#[cfg(target_os = "linux")]
// Shared packet export is used only by Linux zero-copy codec feature combinations.
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

    fn memory_stats(&self) -> Option<BufferPoolStats> {
        None
    }

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
                accepted_inputs: if descriptor.input.is_compressed() {
                    PACKET_OR_HOST_INPUTS
                } else {
                    HOST_OR_DMA_INPUTS
                },
                possible_outputs: HOST_OR_DMA_OUTPUTS,
                preserves_input_residency: false,
            },
            CodecKind::Encoder => CodecResidencyCapabilities {
                accepted_inputs: HOST_OR_DMA_INPUTS,
                possible_outputs: if descriptor.output.is_compressed() {
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
#[cfg(all(feature = "codec-mozjpeg", not(feature = "codec-turbojpeg")))]
pub mod jpeg_encoder;
#[cfg(feature = "codec-jpeg-decoder")]
pub mod mjpeg;
#[cfg(feature = "codec-turbojpeg")]
pub mod mjpeg_turbojpeg;
#[cfg(feature = "codec-zune")]
pub mod mjpeg_zune;
pub mod prelude;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "raw-decoders")]
    use std::sync::Arc;

    #[cfg(feature = "raw-decoders")]
    struct Rg24Passthrough {
        descriptor: CodecDescriptor,
    }

    #[cfg(feature = "raw-decoders")]
    impl Default for Rg24Passthrough {
        fn default() -> Self {
            Self {
                descriptor: CodecDescriptor {
                    kind: CodecKind::Encoder,
                    input: FourCc::RG24,
                    output: FourCc::RG24,
                    name: "passthrough",
                    impl_name: "test",
                },
            }
        }
    }

    #[cfg(feature = "raw-decoders")]
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

    #[cfg(feature = "raw-decoders")]
    #[test]
    fn auto_converts_rgba_to_rg24_for_rg24_codecs() {
        let registry = CodecRegistry::with_enabled_codecs_for_max(8, 8).expect("registry");
        registry.register(FourCc::RG24, Arc::new(Rg24Passthrough::default()));
        let handle = registry.handle();

        let res = Resolution::new(2, 2).unwrap();
        let layout = plane_layout_from_dims(res.width, res.height, 4);
        let pool = BufferPool::with_limits(1, layout.len, 4);
        let mut buf = pool.lease();
        buf.resize(layout.len);
        for (i, b) in buf.as_mut_slice().iter_mut().enumerate() {
            *b = i as u8;
        }
        let format = MediaFormat::new(FourCc::RGBA, res, ColorSpace::Srgb);
        let frame =
            FrameLease::single_plane(FrameMeta::new(format, 0), buf, layout.len, layout.stride);

        let out = handle
            .process_named(FourCc::RG24, "test", frame)
            .expect("process");
        assert_eq!(out.meta().format.code, FourCc::RG24);
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

    #[test]
    fn codec_implementation_id_normalizes_and_classifies_hardware() {
        let id = CodecImplementationId::new(" FFMPEG-VAAPI ");
        assert_eq!(id.as_str(), "ffmpeg-vaapi");
        assert!(id.is_hardware_accelerated());

        let descriptor = CodecDescriptor {
            kind: CodecKind::Decoder,
            input: FourCc::MJPG,
            output: FourCc::RG24,
            name: "mjpeg",
            impl_name: "jpeg-decoder",
        };
        assert_eq!(descriptor.implementation_id().as_str(), "jpeg-decoder");
        assert!(!descriptor.is_hardware_accelerated());
    }

    #[test]
    fn codec_kind_roundtrips_stable_api_strings() {
        assert_eq!(CodecKind::Encoder.to_string(), "encoder");
        assert_eq!("decode".parse::<CodecKind>(), Ok(CodecKind::Decoder));
        assert!("unknown".parse::<CodecKind>().is_err());
    }
}
