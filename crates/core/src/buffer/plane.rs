//! Plane layouts and frame descriptions: plain data, `no_std`.

use smallvec::SmallVec;

use super::meta::{FrameMeta, FrameMutability, FrameResidency};
use crate::format::MediaFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct PlaneLayout {
    pub offset: usize,
    pub len: usize,
    pub stride: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct FramePlaneDescriptor {
    pub offset: usize,
    pub len: usize,
    pub stride: usize,
}

impl From<PlaneLayout> for FramePlaneDescriptor {
    fn from(layout: PlaneLayout) -> Self {
        Self {
            offset: layout.offset,
            len: layout.len,
            stride: layout.stride,
        }
    }
}

impl From<FramePlaneDescriptor> for PlaneLayout {
    fn from(plane: FramePlaneDescriptor) -> Self {
        Self {
            offset: plane.offset,
            len: plane.len,
            stride: plane.stride,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct FrameLeaseDescriptor {
    pub width: u32,
    pub height: u32,
    pub fourcc: crate::format::FourCc,
    pub timestamp: u64,
    pub color: crate::format::ColorSpace,
    /// Inline for up to four planes: building a descriptor does not allocate.
    #[cfg_attr(feature = "schema", schema(value_type = Vec<FramePlaneDescriptor>))]
    pub planes: SmallVec<[FramePlaneDescriptor; 4]>,
}

impl FrameLeaseDescriptor {
    pub fn to_meta(&self) -> Option<FrameMeta> {
        let resolution = crate::format::Resolution::new(self.width, self.height)?;
        let format = crate::format::MediaFormat::new(self.fourcc, resolution, self.color);
        Some(FrameMeta::new(format, self.timestamp))
    }

    pub fn layouts(&self) -> SmallVec<[PlaneLayout; 3]> {
        self.planes.iter().copied().map(PlaneLayout::from).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameAllocation {
    pub format: MediaFormat,
    pub timestamp: u64,
    pub mutability: FrameMutability,
    pub residency: FrameResidency,
    pub stride_alignment: Option<usize>,
    pub plane_alignment: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameValidationError {
    #[error("frame is not host-readable: residency is {0}")]
    NotHostReadable(FrameResidency),
    #[error("frame is not host-writable: residency is {0}")]
    NotHostWritable(FrameResidency),
    #[error("frame is not mutable: mutability is {0:?}")]
    NotMutable(FrameMutability),
    #[error("frame has no planes")]
    NoPlanes,
    #[error("plane count mismatch: expected {expected}, actual {actual}")]
    PlaneCountMismatch { expected: usize, actual: usize },
    #[error("plane {index} stride {stride} is smaller than visible row bytes {visible_row_bytes}")]
    PlaneStrideTooSmall {
        index: usize,
        stride: usize,
        visible_row_bytes: usize,
    },
    #[error("plane {index} length {len} is smaller than expected length {expected_len}")]
    PlaneLenTooSmall {
        index: usize,
        len: usize,
        expected_len: usize,
    },
    #[error("frame width or height is zero")]
    ZeroDimensions,
    #[error("visible byte length mismatch: expected {expected}, actual {actual}")]
    VisibleLenMismatch { expected: usize, actual: usize },
    #[error("frame format has unknown storage layout")]
    UnknownStorageLayout,
    #[error("frame allocation requires host-owned residency, got {0}")]
    UnsupportedAllocationResidency(FrameResidency),
    #[error("frame alias relationship cannot be determined for this backing")]
    AliasUnknown,
    #[error("buffer is too small: expected at least {expected} bytes, got {actual}")]
    BufferTooSmall { expected: usize, actual: usize },
    #[error("plane byte ranges overlap: plane {left} overlaps plane {right}")]
    PlaneRangeOverlap { left: usize, right: usize },
    #[error("alignment must be a non-zero power of two, got {0}")]
    InvalidAlignment(usize),
    #[error("format {0} has no zero-copy luma plane")]
    NoLumaPlane(crate::format::FourCc),
    #[error("companion timestamp {companion} does not match frame timestamp {frame}")]
    CompanionTimestampMismatch { frame: u64, companion: u64 },
    #[error("companion frames cannot carry companions of their own")]
    NestedCompanion,
}
