use std::{fmt, time::Instant};

use crate::format::MediaFormat;

/// Runtime-visible residency for a frame payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameResidency {
    HostOwned,
    HostExternal,
    Dmabuf,
    GpuTexture,
    CompressedPacket,
}

impl fmt::Display for FrameResidency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostOwned => write!(f, "host_owned"),
            Self::HostExternal => write!(f, "host_external"),
            Self::Dmabuf => write!(f, "dmabuf"),
            Self::GpuTexture => write!(f, "gpu_texture"),
            Self::CompressedPacket => write!(f, "compressed_packet"),
        }
    }
}

/// Mutability contract visible to the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FrameMutability {
    #[default]
    Mutable,
    ReadOnly,
}

/// Reason a frame changed residency or had to be materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResidencyTransitionReason {
    Capture,
    Decode,
    Encode,
    FrameHook,
    ImageHook,
    PackedTransform,
    ImageMaterialize,
    FileReplay,
    NetcamIngress,
    BackendFallbackCopy,
    Unknown,
}

impl fmt::Display for ResidencyTransitionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture => write!(f, "capture"),
            Self::Decode => write!(f, "decode"),
            Self::Encode => write!(f, "encode"),
            Self::FrameHook => write!(f, "frame_hook"),
            Self::ImageHook => write!(f, "image_hook"),
            Self::PackedTransform => write!(f, "packed_transform"),
            Self::ImageMaterialize => write!(f, "image_materialize"),
            Self::FileReplay => write!(f, "file_replay"),
            Self::NetcamIngress => write!(f, "netcam_ingress"),
            Self::BackendFallbackCopy => write!(f, "backend_fallback_copy"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Diagnostic record describing the last residency transition on a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResidencyTransition {
    pub from: FrameResidency,
    pub to: FrameResidency,
    pub reason: ResidencyTransitionReason,
    pub copied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendFrameMeta {
    V4l2(V4l2FrameMeta),
}

impl BackendFrameMeta {
    pub fn as_v4l2(&self) -> Option<&V4l2FrameMeta> {
        match self {
            Self::V4l2(meta) => Some(meta),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V4l2FrameMeta {
    pub sequence: u32,
    pub bytes_used: u32,
    pub field: u32,
    pub flags: u32,
    pub zero_copy: bool,
}

/// Metadata associated with a frame.
#[derive(Debug, Clone)]
pub struct FrameMeta {
    pub format: MediaFormat,
    pub timestamp: u64,
    pub backend: Option<BackendFrameMeta>,
    pub capture_instant: Option<Instant>,
    pub residency: Option<FrameResidency>,
    pub mutability: FrameMutability,
    pub last_transition: Option<ResidencyTransition>,
}

impl FrameMeta {
    pub fn new(format: MediaFormat, timestamp: u64) -> Self {
        Self {
            format,
            timestamp,
            backend: None,
            capture_instant: None,
            residency: None,
            mutability: FrameMutability::Mutable,
            last_transition: None,
        }
    }

    pub fn with_backend(mut self, backend: BackendFrameMeta) -> Self {
        self.backend = Some(backend);
        self
    }

    pub fn backend(&self) -> Option<&BackendFrameMeta> {
        self.backend.as_ref()
    }

    pub fn v4l2(&self) -> Option<&V4l2FrameMeta> {
        self.backend.as_ref().and_then(BackendFrameMeta::as_v4l2)
    }

    pub fn with_capture_instant(mut self, capture_instant: Instant) -> Self {
        self.capture_instant = Some(capture_instant);
        self
    }

    pub fn capture_instant(&self) -> Option<Instant> {
        self.capture_instant
    }

    pub fn with_residency(mut self, residency: FrameResidency) -> Self {
        self.residency = Some(residency);
        self
    }

    pub fn residency(&self) -> Option<FrameResidency> {
        self.residency
    }

    pub fn with_mutability(mut self, mutability: FrameMutability) -> Self {
        self.mutability = mutability;
        self
    }

    pub fn mutability(&self) -> FrameMutability {
        self.mutability
    }

    pub fn with_transition(mut self, transition: ResidencyTransition) -> Self {
        self.last_transition = Some(transition);
        self
    }

    pub fn last_transition(&self) -> Option<ResidencyTransition> {
        self.last_transition
    }
}
