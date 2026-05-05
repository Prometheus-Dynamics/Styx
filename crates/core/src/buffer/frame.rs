use smallvec::{SmallVec, smallvec};
use std::{num::NonZeroU32, sync::Arc};

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};

use super::meta::{FrameMeta, FrameMutability, FrameResidency};
#[cfg(target_os = "linux")]
use super::pool::SharedBufferLease;
use super::pool::{BufferLease, BufferPool};
use crate::format::{ChromaSubsampling, FrameLayoutInfo, FrameStorageKind, MediaFormat};

#[cfg(unix)]
mod shared_fd;
#[cfg(unix)]
use shared_fd::SharedFdBacking;
#[cfg(target_os = "linux")]
use shared_fd::create_memfd;

/// External backing for frames when zero-copy sharing external memory.
pub trait ExternalBacking: Send + Sync {
    fn plane_data(&self, index: usize) -> Option<&[u8]>;

    fn backing_bytes(&self) -> Option<usize> {
        None
    }

    fn backing_kind(&self) -> &'static str {
        "external"
    }

    fn can_export(&self) -> bool {
        false
    }

    fn residency(&self) -> FrameResidency {
        FrameResidency::HostExternal
    }

    #[cfg(unix)]
    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
}

#[derive(Debug)]
pub struct PlaneMut<'a> {
    data: &'a mut [u8],
    stride: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRows<'a> {
    data: &'a [u8],
    stride: usize,
    row_bytes: usize,
    rows: usize,
}

#[derive(Debug)]
pub struct VisibleRowsMut<'a> {
    data: &'a mut [u8],
    stride: usize,
    row_bytes: usize,
    rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRow<'a> {
    data: &'a [u8],
}

#[derive(Debug)]
pub struct VisibleRowMut<'a> {
    data: &'a mut [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePlaneShape {
    pub width: usize,
    pub height: usize,
    pub row_bytes: usize,
    pub stride: usize,
    pub offset: usize,
    pub len: usize,
}

impl<'a> Plane<'a> {
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
}

impl<'a> PlaneMut<'a> {
    pub fn data(&mut self) -> &mut [u8] {
        self.data
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
}

impl<'a> VisibleRows<'a> {
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn row_bytes(&self) -> usize {
        self.row_bytes
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn visible_len(&self) -> usize {
        self.row_bytes.saturating_mul(self.rows)
    }

    pub fn row(&self, index: usize) -> Option<VisibleRow<'a>> {
        if index >= self.rows {
            return None;
        }
        let start = index.checked_mul(self.stride)?;
        let end = start.checked_add(self.row_bytes)?;
        Some(VisibleRow {
            data: self.data.get(start..end)?,
        })
    }

    pub fn iter(&self) -> VisibleRowsIter<'a> {
        VisibleRowsIter {
            rows: *self,
            index: 0,
        }
    }
}

impl<'a> IntoIterator for VisibleRows<'a> {
    type Item = VisibleRow<'a>;
    type IntoIter = VisibleRowsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        VisibleRowsIter {
            rows: self,
            index: 0,
        }
    }
}

pub struct VisibleRowsIter<'a> {
    rows: VisibleRows<'a>,
    index: usize,
}

impl<'a> Iterator for VisibleRowsIter<'a> {
    type Item = VisibleRow<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let row = self.rows.row(self.index)?;
        self.index += 1;
        Some(row)
    }
}

impl<'a> VisibleRowsMut<'a> {
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn row_bytes(&self) -> usize {
        self.row_bytes
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn visible_len(&self) -> usize {
        self.row_bytes.saturating_mul(self.rows)
    }

    pub fn for_each_row_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(usize, VisibleRowMut<'_>),
    {
        for index in 0..self.rows {
            let start = index * self.stride;
            let end = start + self.row_bytes;
            if let Some(data) = self.data.get_mut(start..end) {
                f(index, VisibleRowMut { data });
            }
        }
    }
}

impl<'a> VisibleRow<'a> {
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

impl<'a> VisibleRowMut<'a> {
    pub fn data(&mut self) -> &mut [u8] {
        self.data
    }
}

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
    pub planes: Vec<FramePlaneDescriptor>,
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

#[derive(Debug, thiserror::Error)]
pub enum FrameExportError {
    #[error("frame backing is process-local and cannot be exported without copying")]
    NotExportable,
    #[error("fd operation failed: {0}")]
    Fd(std::io::Error),
    #[error("fd mmap failed: {0}")]
    Mmap(std::io::Error),
    #[error("frame descriptor is invalid")]
    InvalidDescriptor,
    #[error("plane count mismatch: descriptor has {expected}, backing has {actual}")]
    PlaneCountMismatch { expected: usize, actual: usize },
}

#[cfg(unix)]
#[derive(Debug)]
pub struct FrameFdPlane {
    pub fd: OwnedFd,
    pub offset: usize,
    pub len: usize,
}

#[cfg(unix)]
#[derive(Debug)]
pub enum FrameBackingExport {
    Memfd { fd: OwnedFd, len: usize },
    DmabufPlanes { planes: Vec<FrameFdPlane> },
}

pub struct FrameLease {
    meta: FrameMeta,
    buffers: SmallVec<[BufferLease; 3]>,
    layouts: SmallVec<[PlaneLayout; 3]>,
    external: Option<Arc<dyn ExternalBacking>>,
}

pub struct FrameLeaseParts {
    pub meta: FrameMeta,
    pub layouts: SmallVec<[PlaneLayout; 3]>,
    pub buffers: SmallVec<[Vec<u8>; 3]>,
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
}

impl FrameLease {
    pub fn single_plane(
        mut meta: FrameMeta,
        mut buffer: BufferLease,
        len: usize,
        stride: usize,
    ) -> Self {
        buffer.resize(len);
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            layouts: smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffers: smallvec![buffer],
            external: None,
        }
    }

    /// # Safety
    /// The caller must write every byte of the buffer before the frame is read.
    pub unsafe fn single_plane_uninit(
        mut meta: FrameMeta,
        mut buffer: BufferLease,
        len: usize,
        stride: usize,
    ) -> Self {
        unsafe { buffer.resize_uninit(len) };
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            layouts: smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffers: smallvec![buffer],
            external: None,
        }
    }

    pub fn multi_plane(
        mut meta: FrameMeta,
        buffers: SmallVec<[BufferLease; 3]>,
        layouts: SmallVec<[PlaneLayout; 3]>,
    ) -> Self {
        debug_assert_eq!(buffers.len(), layouts.len());
        if meta.residency.is_none() {
            meta.residency = Some(default_owned_residency(meta.format.code));
        }
        Self {
            meta,
            buffers,
            layouts,
            external: None,
        }
    }

    pub fn from_external(
        mut meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        backing: Arc<dyn ExternalBacking>,
    ) -> Self {
        if meta.residency.is_none() {
            meta.residency = Some(backing.residency());
        }
        meta.mutability = FrameMutability::ReadOnly;
        Self {
            meta,
            buffers: SmallVec::new(),
            layouts,
            external: Some(backing),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn single_plane_shared(
        mut meta: FrameMeta,
        mut buffer: SharedBufferLease,
        len: usize,
        stride: usize,
    ) -> Result<Self, FrameExportError> {
        buffer.try_resize(len)?;
        if meta.residency.is_none() {
            meta.residency = Some(FrameResidency::HostExternal);
        }
        Ok(Self::from_external(
            meta,
            smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffer.into_external_backing(1),
        ))
    }

    #[cfg(target_os = "linux")]
    pub fn multi_plane_shared(
        mut meta: FrameMeta,
        mut buffer: SharedBufferLease,
        layouts: SmallVec<[PlaneLayout; 3]>,
    ) -> Result<Self, FrameExportError> {
        let len = layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(0);
        buffer.try_resize(len)?;
        if meta.residency.is_none() {
            meta.residency = Some(FrameResidency::HostExternal);
        }
        let plane_count = layouts.len();
        Ok(Self::from_external(
            meta,
            layouts,
            buffer.into_external_backing(plane_count),
        ))
    }

    #[cfg(unix)]
    pub fn from_shared_fd(
        meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        fd: OwnedFd,
    ) -> Self {
        Self::from_memfd(meta, layouts, fd)
    }

    #[cfg(unix)]
    pub fn from_memfd(
        mut meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        fd: OwnedFd,
    ) -> Self {
        meta.residency = Some(FrameResidency::HostExternal);
        let len = layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(0);
        let backing = SharedFdBacking::memfd(fd, len, layouts.len());
        Self::from_external(meta, layouts, Arc::new(backing))
    }

    #[cfg(unix)]
    pub fn from_dmabuf(
        mut meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        planes: Vec<FrameFdPlane>,
    ) -> Result<Self, FrameExportError> {
        if planes.len() != layouts.len() {
            return Err(FrameExportError::PlaneCountMismatch {
                expected: layouts.len(),
                actual: planes.len(),
            });
        }
        meta.residency = Some(FrameResidency::Dmabuf);
        let backing = SharedFdBacking::dmabuf(planes);
        Ok(Self::from_external(meta, layouts, Arc::new(backing)))
    }

    #[cfg(unix)]
    pub fn from_memfd_import(
        descriptor: FrameLeaseDescriptor,
        fd: OwnedFd,
    ) -> Result<Self, FrameExportError> {
        let meta = descriptor
            .to_meta()
            .ok_or(FrameExportError::InvalidDescriptor)?;
        Ok(Self::from_memfd(meta, descriptor.layouts(), fd))
    }

    #[cfg(unix)]
    pub fn from_dmabuf_import(
        descriptor: FrameLeaseDescriptor,
        planes: Vec<FrameFdPlane>,
    ) -> Result<Self, FrameExportError> {
        let meta = descriptor
            .to_meta()
            .ok_or(FrameExportError::InvalidDescriptor)?;
        Self::from_dmabuf(meta, descriptor.layouts(), planes)
    }

    pub fn allocate_host_owned(
        format: MediaFormat,
        timestamp: u64,
    ) -> Result<Self, FrameValidationError> {
        Self::allocate(FrameAllocation {
            format,
            timestamp,
            mutability: FrameMutability::Mutable,
            residency: FrameResidency::HostOwned,
            stride_alignment: None,
            plane_alignment: None,
        })
    }

    pub fn allocate_same_layout(&self) -> Result<Self, FrameValidationError> {
        let mut frame = Self::allocate(FrameAllocation {
            format: self.meta.format,
            timestamp: self.meta.timestamp,
            mutability: self.meta.mutability,
            residency: FrameResidency::HostOwned,
            stride_alignment: None,
            plane_alignment: None,
        })?;
        frame.layouts = self.layouts.clone();
        let max_len = frame
            .layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(1)
            .max(1);
        let pool =
            BufferPool::with_limits(frame.layouts.len().max(1), max_len, frame.layouts.len());
        frame.buffers = frame
            .layouts
            .iter()
            .map(|layout| {
                let mut lease = pool.lease();
                lease.resize(layout.offset.saturating_add(layout.len));
                lease
            })
            .collect();
        Ok(frame)
    }

    pub fn allocate_like(&self, format: MediaFormat) -> Result<Self, FrameValidationError> {
        Self::allocate(FrameAllocation {
            format,
            timestamp: self.meta.timestamp,
            mutability: FrameMutability::Mutable,
            residency: FrameResidency::HostOwned,
            stride_alignment: None,
            plane_alignment: None,
        })
    }

    pub fn allocate_like_layout_with_timestamp(
        &self,
        timestamp: u64,
    ) -> Result<Self, FrameValidationError> {
        let mut frame = self.allocate_same_layout()?;
        frame.meta.timestamp = timestamp;
        Ok(frame)
    }

    pub fn allocate(allocation: FrameAllocation) -> Result<Self, FrameValidationError> {
        if allocation.residency != FrameResidency::HostOwned {
            return Err(FrameValidationError::UnsupportedAllocationResidency(
                allocation.residency,
            ));
        }
        let meta = FrameMeta::new(allocation.format, allocation.timestamp)
            .with_residency(FrameResidency::HostOwned)
            .with_mutability(allocation.mutability);
        validate_alignment(allocation.stride_alignment)?;
        validate_alignment(allocation.plane_alignment)?;
        let layouts = default_layouts_for_format(
            allocation.format,
            allocation.stride_alignment,
            allocation.plane_alignment,
        )?;
        let max_len = layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(1)
            .max(1);
        let pool = BufferPool::with_limits(layouts.len().max(1), max_len, layouts.len());
        let buffers = layouts
            .iter()
            .map(|layout| {
                let mut lease = pool.lease();
                lease.resize(layout.offset.saturating_add(layout.len));
                lease
            })
            .collect();
        Ok(Self::multi_plane(meta, buffers, layouts))
    }

    pub fn meta(&self) -> &FrameMeta {
        &self.meta
    }

    pub fn meta_mut(&mut self) -> &mut FrameMeta {
        &mut self.meta
    }

    pub fn is_external(&self) -> bool {
        self.external.is_some()
    }

    pub fn can_read_planes(&self) -> bool {
        self.has_host_readable_bytes()
    }

    pub fn can_write_planes(&self) -> bool {
        self.has_host_writable_bytes()
    }

    pub fn can_export(&self) -> bool {
        self.external
            .as_ref()
            .is_some_and(|backing| backing.can_export())
    }

    pub fn can_materialize_without_copy(&self) -> bool {
        matches!(self.residency(), FrameResidency::HostOwned) && self.external.is_none()
    }

    pub fn residency(&self) -> FrameResidency {
        self.meta
            .residency
            .unwrap_or_else(|| match self.external.as_ref() {
                Some(backing) => backing.residency(),
                None => default_owned_residency(self.meta.format.code),
            })
    }

    pub fn mutability(&self) -> FrameMutability {
        self.meta.mutability
    }

    pub fn payload_bytes(&self) -> usize {
        self.layouts.iter().map(|layout| layout.len).sum()
    }

    pub fn visible_payload_bytes(&self) -> Result<usize, FrameValidationError> {
        let mut bytes = 0usize;
        for index in 0..self.layouts.len() {
            let shape = self.plane_shape(index)?;
            bytes = bytes
                .checked_add(shape.row_bytes.saturating_mul(shape.height))
                .ok_or(FrameValidationError::UnknownStorageLayout)?;
        }
        Ok(bytes)
    }

    pub fn is_tightly_packed(&self) -> Result<bool, FrameValidationError> {
        for index in 0..self.layouts.len() {
            let shape = self.plane_shape(index)?;
            if shape.stride != shape.row_bytes
                || shape.len < shape.row_bytes.saturating_mul(shape.height)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn descriptor(&self) -> FrameLeaseDescriptor {
        let format = self.meta.format;
        FrameLeaseDescriptor {
            width: format.resolution.width.get(),
            height: format.resolution.height.get(),
            fourcc: format.code,
            timestamp: self.meta.timestamp,
            color: format.color,
            planes: self.layouts.iter().copied().map(Into::into).collect(),
        }
    }

    pub fn layout_info(&self) -> FrameLayoutInfo {
        self.meta.format.code.layout_info()
    }

    pub fn has_host_readable_bytes(&self) -> bool {
        matches!(
            self.residency(),
            FrameResidency::HostOwned
                | FrameResidency::HostExternal
                | FrameResidency::CompressedPacket
        )
    }

    pub fn has_host_writable_bytes(&self) -> bool {
        matches!(
            self.residency(),
            FrameResidency::HostOwned | FrameResidency::CompressedPacket
        ) && self.mutability() == FrameMutability::Mutable
            && self.external.is_none()
    }

    pub fn require_host_readable(&self) -> Result<(), FrameValidationError> {
        if self.has_host_readable_bytes() {
            Ok(())
        } else {
            Err(FrameValidationError::NotHostReadable(self.residency()))
        }
    }

    pub fn require_host_writable(&self) -> Result<(), FrameValidationError> {
        if self.mutability() != FrameMutability::Mutable {
            return Err(FrameValidationError::NotMutable(self.mutability()));
        }
        if self.has_host_writable_bytes() {
            Ok(())
        } else {
            Err(FrameValidationError::NotHostWritable(self.residency()))
        }
    }

    pub fn first_plane_visible_row_bytes(&self) -> Option<usize> {
        let format = self.meta.format;
        self.layout_info()
            .first_plane_visible_row_bytes(format.resolution.width.get() as usize)
    }

    pub fn plane_shape(&self, plane_index: usize) -> Result<FramePlaneShape, FrameValidationError> {
        let format = self.meta.format;
        let width = format.resolution.width.get() as usize;
        let height = format.resolution.height.get() as usize;
        let info = self.layout_info();
        let layout = self
            .layouts
            .get(plane_index)
            .ok_or(FrameValidationError::NoPlanes)?;
        let row_bytes = visible_row_bytes_for_plane(info, width, plane_index)
            .ok_or(FrameValidationError::UnknownStorageLayout)?;
        let rows = visible_rows_for_plane(info, height, plane_index)
            .ok_or(FrameValidationError::UnknownStorageLayout)?;
        let plane_width = visible_width_for_plane(info, width, plane_index)
            .ok_or(FrameValidationError::UnknownStorageLayout)?;
        Ok(FramePlaneShape {
            width: plane_width,
            height: rows,
            row_bytes,
            stride: layout.stride,
            offset: layout.offset,
            len: layout.len,
        })
    }

    pub fn validate_plane_layouts(&self) -> Result<(), FrameValidationError> {
        let format = self.meta.format;
        let width = format.resolution.width.get() as usize;
        let height = format.resolution.height.get() as usize;
        if width == 0 || height == 0 {
            return Err(FrameValidationError::ZeroDimensions);
        }
        let info = self.layout_info();
        if info.planes.planes > 0 && self.layouts.len() != info.planes.planes {
            return Err(FrameValidationError::PlaneCountMismatch {
                expected: info.planes.planes,
                actual: self.layouts.len(),
            });
        }
        validate_layouts_for_info(&self.layouts, info, width, height)?;
        if self.uses_shared_plane_address_space() {
            validate_plane_ranges_do_not_overlap(&self.layouts)?;
        }
        Ok(())
    }

    pub fn planes_visible(&self) -> Result<SmallVec<[VisibleRows<'_>; 3]>, FrameValidationError> {
        let mut planes = SmallVec::with_capacity(self.layouts.len());
        for index in 0..self.layouts.len() {
            planes.push(self.visible_rows(index)?);
        }
        Ok(planes)
    }

    pub fn visible_rows(
        &self,
        plane_index: usize,
    ) -> Result<VisibleRows<'_>, FrameValidationError> {
        self.require_host_readable()?;
        self.validate_plane_layouts()?;
        let row_bytes = visible_row_bytes_for_plane(
            self.layout_info(),
            self.meta.format.resolution.width.get() as usize,
            plane_index,
        )
        .ok_or(FrameValidationError::UnknownStorageLayout)?;
        let row_count = visible_rows_for_plane(
            self.layout_info(),
            self.meta.format.resolution.height.get() as usize,
            plane_index,
        )
        .ok_or(FrameValidationError::UnknownStorageLayout)?;
        let Some(layout) = self.layouts.get(plane_index).copied() else {
            return Err(FrameValidationError::NoPlanes);
        };
        let data = if let Some(backing) = &self.external {
            backing
                .plane_data(plane_index)
                .and_then(|data| data.get(layout.offset..layout.offset.saturating_add(layout.len)))
        } else {
            self.buffers.get(plane_index).and_then(|buffer| {
                buffer
                    .as_slice()
                    .get(layout.offset..layout.offset.saturating_add(layout.len))
            })
        }
        .ok_or(FrameValidationError::NoPlanes)?;
        Ok(VisibleRows {
            data,
            stride: layout.stride,
            row_bytes,
            rows: row_count,
        })
    }

    pub fn try_as_contiguous_visible_plane(
        &self,
        plane_index: usize,
    ) -> Result<Option<&[u8]>, FrameValidationError> {
        let rows = self.visible_rows(plane_index)?;
        if rows.stride != rows.row_bytes {
            return Ok(None);
        }
        let len = rows.visible_len();
        Ok(Some(rows.data.get(0..len).ok_or(
            FrameValidationError::BufferTooSmall {
                expected: len,
                actual: rows.data.len(),
            },
        )?))
    }

    pub fn try_as_contiguous_visible_plane_mut(
        &mut self,
        plane_index: usize,
    ) -> Result<Option<&mut [u8]>, FrameValidationError> {
        let rows = self.visible_rows_mut(plane_index)?;
        if rows.stride != rows.row_bytes {
            return Ok(None);
        }
        let len = rows.visible_len();
        let actual = rows.data.len();
        Ok(Some(rows.data.get_mut(0..len).ok_or(
            FrameValidationError::BufferTooSmall {
                expected: len,
                actual,
            },
        )?))
    }

    pub fn copy_visible_plane_to_slice(
        &self,
        plane_index: usize,
        dst: &mut [u8],
    ) -> Result<usize, FrameValidationError> {
        let rows = self.visible_rows(plane_index)?;
        let len = rows.visible_len();
        if dst.len() < len {
            return Err(FrameValidationError::BufferTooSmall {
                expected: len,
                actual: dst.len(),
            });
        }
        if rows.stride == rows.row_bytes {
            dst[..len].copy_from_slice(rows.data.get(0..len).ok_or(
                FrameValidationError::BufferTooSmall {
                    expected: len,
                    actual: rows.data.len(),
                },
            )?);
            return Ok(len);
        }
        let mut offset = 0usize;
        for row in rows {
            let data = row.data();
            dst[offset..offset + data.len()].copy_from_slice(data);
            offset += data.len();
        }
        Ok(len)
    }

    pub fn copy_slice_to_visible_plane(
        &mut self,
        plane_index: usize,
        src: &[u8],
    ) -> Result<usize, FrameValidationError> {
        let mut rows = self.visible_rows_mut(plane_index)?;
        let len = rows.visible_len();
        if src.len() < len {
            return Err(FrameValidationError::BufferTooSmall {
                expected: len,
                actual: src.len(),
            });
        }
        if rows.stride == rows.row_bytes {
            let actual = rows.data.len();
            rows.data
                .get_mut(0..len)
                .ok_or(FrameValidationError::BufferTooSmall {
                    expected: len,
                    actual,
                })?
                .copy_from_slice(&src[..len]);
            return Ok(len);
        }
        let mut offset = 0usize;
        rows.for_each_row_mut(|_, mut row| {
            let data = row.data();
            data.copy_from_slice(&src[offset..offset + data.len()]);
            offset += data.len();
        });
        Ok(len)
    }

    pub fn visible_rows_mut(
        &mut self,
        plane_index: usize,
    ) -> Result<VisibleRowsMut<'_>, FrameValidationError> {
        self.require_host_writable()?;
        self.validate_plane_layouts()?;
        let row_bytes = visible_row_bytes_for_plane(
            self.layout_info(),
            self.meta.format.resolution.width.get() as usize,
            plane_index,
        )
        .ok_or(FrameValidationError::UnknownStorageLayout)?;
        let row_count = visible_rows_for_plane(
            self.layout_info(),
            self.meta.format.resolution.height.get() as usize,
            plane_index,
        )
        .ok_or(FrameValidationError::UnknownStorageLayout)?;
        let Some(layout) = self.layouts.get(plane_index).copied() else {
            return Err(FrameValidationError::NoPlanes);
        };
        let Some(buffer) = self.buffers.get_mut(plane_index) else {
            return Err(FrameValidationError::NoPlanes);
        };
        let end = layout.offset.saturating_add(layout.len);
        if buffer.len() < end {
            buffer.resize(end);
        }
        let data = buffer
            .as_mut_slice()
            .get_mut(layout.offset..end)
            .ok_or(FrameValidationError::NoPlanes)?;
        Ok(VisibleRowsMut {
            data,
            stride: layout.stride,
            row_bytes,
            rows: row_count,
        })
    }

    pub fn may_alias(&self, other: &Self) -> Result<bool, FrameValidationError> {
        if let (Some(left), Some(right)) = (&self.external, &other.external) {
            return Ok(Arc::ptr_eq(left, right));
        }
        if self.external.is_some() || other.external.is_some() {
            return Err(FrameValidationError::AliasUnknown);
        }
        for left in &self.buffers {
            let left = left.as_slice();
            if left.is_empty() {
                continue;
            }
            let left_start = left.as_ptr() as usize;
            let left_end = left_start.saturating_add(left.len());
            for right in &other.buffers {
                let right = right.as_slice();
                if right.is_empty() {
                    continue;
                }
                let right_start = right.as_ptr() as usize;
                let right_end = right_start.saturating_add(right.len());
                if left_start < right_end && right_start < left_end {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn external_backing_bytes(&self) -> Option<usize> {
        self.external.as_ref().map(|backing| {
            backing
                .backing_bytes()
                .unwrap_or_else(|| self.payload_bytes())
        })
    }

    pub fn external_backing_kind(&self) -> Option<&'static str> {
        self.external.as_ref().map(|backing| backing.backing_kind())
    }

    pub fn planes(&self) -> SmallVec<[Plane<'_>; 3]> {
        if let Some(backing) = &self.external {
            self.layouts
                .iter()
                .enumerate()
                .map(|(idx, layout)| {
                    let slice = backing
                        .plane_data(idx)
                        .map(|s| {
                            let end = layout.offset.saturating_add(layout.len);
                            s.get(layout.offset..end).unwrap_or(&[])
                        })
                        .unwrap_or(&[]);
                    Plane {
                        data: slice,
                        stride: layout.stride,
                    }
                })
                .collect()
        } else {
            self.layouts
                .iter()
                .zip(self.buffers.iter())
                .map(|(layout, buf)| {
                    let slice = buf
                        .as_slice()
                        .get(layout.offset..layout.offset + layout.len)
                        .unwrap_or(&[]);
                    Plane {
                        data: slice,
                        stride: layout.stride,
                    }
                })
                .collect()
        }
    }

    pub fn planes_mut(&mut self) -> SmallVec<[PlaneMut<'_>; 3]> {
        if self.external.is_some() {
            return self
                .layouts
                .iter()
                .map(|layout| PlaneMut {
                    data: &mut [],
                    stride: layout.stride,
                })
                .collect();
        }
        self.layouts
            .iter()
            .zip(self.buffers.iter_mut())
            .map(|(layout, buf)| {
                let len = layout.offset + layout.len;
                if buf.len() < len {
                    buf.resize(len);
                }
                let slice = buf
                    .as_mut_slice()
                    .get_mut(layout.offset..layout.offset + layout.len)
                    .unwrap_or(&mut []);
                PlaneMut {
                    data: slice,
                    stride: layout.stride,
                }
            })
            .collect()
    }

    pub fn layouts(&self) -> SmallVec<[PlaneLayout; 3]> {
        self.layouts.clone()
    }

    pub fn layout_slice(&self) -> &[PlaneLayout] {
        &self.layouts
    }

    #[cfg(unix)]
    pub fn export_backing(&self) -> Result<FrameBackingExport, FrameExportError> {
        self.external
            .as_ref()
            .ok_or(FrameExportError::NotExportable)?
            .export_backing()?
            .ok_or(FrameExportError::NotExportable)
    }

    #[cfg(unix)]
    pub fn export_descriptor_and_backing(
        &self,
    ) -> Result<(FrameLeaseDescriptor, FrameBackingExport), FrameExportError> {
        Ok((self.descriptor(), self.export_backing()?))
    }

    #[cfg(target_os = "linux")]
    pub fn export_or_copy_memfd(
        &self,
    ) -> Result<(FrameLeaseDescriptor, FrameBackingExport), FrameExportError> {
        if let Some(backing) = self.external.as_ref()
            && let Some(export) = backing.export_backing()?
        {
            return Ok((self.descriptor(), export));
        }
        Ok((
            self.descriptor(),
            FrameBackingExport::Memfd {
                fd: self.copy_to_memfd()?,
                len: self.backing_span_len(),
            },
        ))
    }

    pub fn plane_strides(&self) -> SmallVec<[usize; 3]> {
        self.layouts.iter().map(|l| l.stride).collect()
    }

    pub fn into_parts(self) -> FrameLeaseParts {
        let layouts = self.layouts.clone();
        if self.external.is_some() {
            FrameLeaseParts {
                meta: self.meta,
                layouts,
                buffers: SmallVec::new(),
            }
        } else {
            let buffers = self.buffers.into_iter().map(|lease| lease.take()).collect();
            FrameLeaseParts {
                meta: self.meta,
                layouts,
                buffers,
            }
        }
    }

    pub fn materialize_owned(&self) -> Self {
        let max_len = self
            .layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(1)
            .max(1);
        let pool = BufferPool::with_limits(self.layouts.len().max(1), max_len, self.layouts.len());
        let buffers = self
            .planes()
            .into_iter()
            .zip(self.layouts.iter().copied())
            .map(|(plane, layout)| {
                let required = layout.offset.saturating_add(layout.len);
                let mut lease = pool.lease();
                lease.resize(required);
                if layout.len > 0 {
                    let dst = &mut lease.as_mut_slice()[layout.offset..required];
                    dst.copy_from_slice(plane.data());
                }
                lease
            })
            .collect();
        let mut meta = self.meta.clone();
        meta.residency = Some(FrameResidency::HostOwned);
        meta.mutability = FrameMutability::Mutable;
        FrameLease::multi_plane(meta, buffers, self.layouts.clone())
    }

    fn backing_span_len(&self) -> usize {
        self.layouts
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .max()
            .unwrap_or(0)
    }

    fn uses_shared_plane_address_space(&self) -> bool {
        if self.buffers.len() == 1 && self.layouts.len() > 1 {
            return true;
        }
        self.external
            .as_ref()
            .is_some_and(|backing| matches!(backing.backing_kind(), "memfd" | "memfd_pool"))
    }

    #[cfg(target_os = "linux")]
    fn copy_to_memfd(&self) -> Result<OwnedFd, FrameExportError> {
        let fd = create_memfd("styx-frame")?;
        let len = self.backing_span_len();
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
        }
        for (plane, layout) in self.planes().into_iter().zip(self.layouts.iter()) {
            let data = plane.data();
            let copy_len = data.len().min(layout.len);
            let mut written = 0usize;
            while written < copy_len {
                let ret = unsafe {
                    libc::pwrite(
                        fd.as_raw_fd(),
                        data[written..copy_len].as_ptr().cast(),
                        copy_len - written,
                        layout.offset.saturating_add(written) as libc::off_t,
                    )
                };
                if ret < 0 {
                    return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
                }
                if ret == 0 {
                    return Err(FrameExportError::Fd(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "short memfd write",
                    )));
                }
                written = written.saturating_add(ret as usize);
            }
        }
        Ok(fd)
    }
}

pub fn plane_layout_from_dims(
    width: NonZeroU32,
    height: NonZeroU32,
    bytes_per_pixel: usize,
) -> PlaneLayout {
    let stride = width.get() as usize * bytes_per_pixel;
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

pub fn plane_layout_with_stride(
    _width: NonZeroU32,
    height: NonZeroU32,
    stride: usize,
) -> PlaneLayout {
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

fn default_layouts_for_format(
    format: MediaFormat,
    stride_alignment: Option<usize>,
    plane_alignment: Option<usize>,
) -> Result<SmallVec<[PlaneLayout; 3]>, FrameValidationError> {
    let width = format.resolution.width.get() as usize;
    let height = format.resolution.height.get() as usize;
    let info = format.code.layout_info();
    match info.storage {
        FrameStorageKind::Packed | FrameStorageKind::RawBayer => {
            let row_bytes = info
                .first_plane_visible_row_bytes(width)
                .ok_or(FrameValidationError::UnknownStorageLayout)?;
            Ok(smallvec![plane_layout_for_visible(
                row_bytes,
                height,
                stride_alignment,
                plane_alignment,
            )?])
        }
        FrameStorageKind::SemiPlanar => match info.planes.subsampling {
            Some(ChromaSubsampling::Cs420) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(
                    width,
                    height.div_ceil(2),
                    stride_alignment,
                    plane_alignment,
                )?,
            ]),
            Some(ChromaSubsampling::Cs422) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
            ]),
            Some(ChromaSubsampling::Cs444) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(width * 2, height, stride_alignment, plane_alignment)?,
            ]),
            None => Err(FrameValidationError::UnknownStorageLayout),
        },
        FrameStorageKind::Planar => match info.planes.subsampling {
            Some(ChromaSubsampling::Cs420) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(
                    width.div_ceil(2),
                    height.div_ceil(2),
                    stride_alignment,
                    plane_alignment,
                )?,
                plane_layout_for_visible(
                    width.div_ceil(2),
                    height.div_ceil(2),
                    stride_alignment,
                    plane_alignment,
                )?,
            ]),
            Some(ChromaSubsampling::Cs422) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(
                    width.div_ceil(2),
                    height,
                    stride_alignment,
                    plane_alignment,
                )?,
                plane_layout_for_visible(
                    width.div_ceil(2),
                    height,
                    stride_alignment,
                    plane_alignment,
                )?,
            ]),
            Some(ChromaSubsampling::Cs444) => Ok(smallvec![
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
                plane_layout_for_visible(width, height, stride_alignment, plane_alignment)?,
            ]),
            None => Ok(smallvec![plane_layout_for_visible(
                width,
                height,
                stride_alignment,
                plane_alignment,
            )?]),
        },
        FrameStorageKind::Compressed | FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            Err(FrameValidationError::UnknownStorageLayout)
        }
    }
}

fn plane_layout_for_visible(
    row_bytes: usize,
    rows: usize,
    stride_alignment: Option<usize>,
    plane_alignment: Option<usize>,
) -> Result<PlaneLayout, FrameValidationError> {
    let stride = align_up(row_bytes, stride_alignment)?;
    let len = align_up(stride.saturating_mul(rows), plane_alignment)?;
    Ok(PlaneLayout {
        offset: 0,
        len,
        stride,
    })
}

fn validate_layouts_for_info(
    layouts: &[PlaneLayout],
    info: FrameLayoutInfo,
    width: usize,
    height: usize,
) -> Result<(), FrameValidationError> {
    match info.storage {
        FrameStorageKind::Compressed => {
            if layouts.is_empty() {
                Err(FrameValidationError::NoPlanes)
            } else {
                Ok(())
            }
        }
        FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            Err(FrameValidationError::UnknownStorageLayout)
        }
        _ => {
            for index in 0..info.planes.planes {
                let row_bytes = visible_row_bytes_for_plane(info, width, index)
                    .ok_or(FrameValidationError::UnknownStorageLayout)?;
                let rows = visible_rows_for_plane(info, height, index)
                    .ok_or(FrameValidationError::UnknownStorageLayout)?;
                validate_plane_layout(index, layouts.get(index), row_bytes, rows)?;
            }
            Ok(())
        }
    }
}

fn validate_plane_ranges_do_not_overlap(
    layouts: &[PlaneLayout],
) -> Result<(), FrameValidationError> {
    for (left_index, left) in layouts.iter().enumerate() {
        let left_start = left.offset;
        let left_end = left.offset.saturating_add(left.len);
        for (right_index, right) in layouts.iter().enumerate().skip(left_index + 1) {
            let right_start = right.offset;
            let right_end = right.offset.saturating_add(right.len);
            if left_start < right_end && right_start < left_end {
                return Err(FrameValidationError::PlaneRangeOverlap {
                    left: left_index,
                    right: right_index,
                });
            }
        }
    }
    Ok(())
}

fn validate_alignment(alignment: Option<usize>) -> Result<(), FrameValidationError> {
    let Some(alignment) = alignment else {
        return Ok(());
    };
    if alignment == 0 || !alignment.is_power_of_two() {
        return Err(FrameValidationError::InvalidAlignment(alignment));
    }
    Ok(())
}

fn align_up(value: usize, alignment: Option<usize>) -> Result<usize, FrameValidationError> {
    let Some(alignment) = alignment else {
        return Ok(value);
    };
    validate_alignment(Some(alignment))?;
    Ok(value.saturating_add(alignment - 1) & !(alignment - 1))
}

fn visible_row_bytes_for_plane(
    info: FrameLayoutInfo,
    width: usize,
    plane_index: usize,
) -> Option<usize> {
    match info.storage {
        FrameStorageKind::Packed | FrameStorageKind::RawBayer => (plane_index == 0)
            .then(|| info.first_plane_visible_row_bytes(width))
            .flatten(),
        FrameStorageKind::SemiPlanar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 0 | 1) => Some(width),
            (Some(ChromaSubsampling::Cs444), 0) => Some(width),
            (Some(ChromaSubsampling::Cs444), 1) => width.checked_mul(2),
            (None, 0) => Some(width),
            _ => None,
        },
        FrameStorageKind::Planar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 0) => Some(width),
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 1 | 2) => {
                Some(width.div_ceil(2))
            }
            (Some(ChromaSubsampling::Cs444), 0..=2) => Some(width),
            (None, 0) => Some(width),
            _ => None,
        },
        FrameStorageKind::Compressed | FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            None
        }
    }
}

fn visible_width_for_plane(
    info: FrameLayoutInfo,
    width: usize,
    plane_index: usize,
) -> Option<usize> {
    match info.storage {
        FrameStorageKind::Packed | FrameStorageKind::RawBayer => {
            (plane_index == 0).then_some(width)
        }
        FrameStorageKind::SemiPlanar => match (info.planes.subsampling, plane_index) {
            (Some(_), 0 | 1) => Some(width),
            (None, 0) => Some(width),
            _ => None,
        },
        FrameStorageKind::Planar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 0) => Some(width),
            (Some(ChromaSubsampling::Cs420 | ChromaSubsampling::Cs422), 1 | 2) => {
                Some(width.div_ceil(2))
            }
            (Some(ChromaSubsampling::Cs444), 0..=2) => Some(width),
            (None, 0) => Some(width),
            _ => None,
        },
        FrameStorageKind::Compressed | FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            None
        }
    }
}

fn visible_rows_for_plane(
    info: FrameLayoutInfo,
    height: usize,
    plane_index: usize,
) -> Option<usize> {
    match info.storage {
        FrameStorageKind::Packed | FrameStorageKind::RawBayer => {
            (plane_index == 0).then_some(height)
        }
        FrameStorageKind::SemiPlanar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420), 0) => Some(height),
            (Some(ChromaSubsampling::Cs420), 1) => Some(height.div_ceil(2)),
            (Some(ChromaSubsampling::Cs422 | ChromaSubsampling::Cs444), 0 | 1) => Some(height),
            (None, 0) => Some(height),
            _ => None,
        },
        FrameStorageKind::Planar => match (info.planes.subsampling, plane_index) {
            (Some(ChromaSubsampling::Cs420), 0) => Some(height),
            (Some(ChromaSubsampling::Cs420), 1 | 2) => Some(height.div_ceil(2)),
            (Some(ChromaSubsampling::Cs422 | ChromaSubsampling::Cs444), 0..=2) => Some(height),
            (None, 0) => Some(height),
            _ => None,
        },
        FrameStorageKind::Compressed | FrameStorageKind::OpaqueGpu | FrameStorageKind::Unknown => {
            None
        }
    }
}

fn validate_plane_layout(
    index: usize,
    layout: Option<&PlaneLayout>,
    visible_row_bytes: usize,
    height: usize,
) -> Result<(), FrameValidationError> {
    let Some(layout) = layout else {
        return Err(FrameValidationError::NoPlanes);
    };
    if layout.stride < visible_row_bytes {
        return Err(FrameValidationError::PlaneStrideTooSmall {
            index,
            stride: layout.stride,
            visible_row_bytes,
        });
    }
    let expected_len = layout.stride.saturating_mul(height);
    if layout.len < expected_len {
        return Err(FrameValidationError::PlaneLenTooSmall {
            index,
            len: layout.len,
            expected_len,
        });
    }
    Ok(())
}

fn default_owned_residency(code: crate::format::FourCc) -> FrameResidency {
    if code.is_compressed() {
        FrameResidency::CompressedPacket
    } else {
        FrameResidency::HostOwned
    }
}
