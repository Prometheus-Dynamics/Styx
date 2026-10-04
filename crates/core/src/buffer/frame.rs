use smallvec::{SmallVec, smallvec};
use std::sync::Arc;

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};

use super::meta::{FrameMeta, FrameMutability, FrameResidency};
#[cfg(target_os = "linux")]
use super::pool::SharedBufferLease;
use super::pool::{BufferLease, BufferPool};
use crate::format::{FrameLayoutInfo, MediaFormat};

mod companion;
mod construct;
mod crop;
mod luma;
mod share;
mod visible;

#[allow(unused_imports)]
use super::layout::*;
use super::plane::{FrameAllocation, FrameLeaseDescriptor, FrameValidationError, PlaneLayout};
use super::views::{FramePlaneShape, Plane, PlaneMut, VisibleRows, VisibleRowsMut};

pub use companion::{CompanionKind, box_downscale_luma, box_downscale_luma_in};
#[cfg(unix)]
mod shared_fd;
#[cfg(target_os = "linux")]
use shared_fd::create_memfd;
#[cfg(unix)]
use shared_fd::{SharedFdBacking, fd_size};

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
    companions: Option<Box<companion::Companions>>,
}

pub struct FrameLeaseParts {
    pub meta: FrameMeta,
    pub layouts: SmallVec<[PlaneLayout; 3]>,
    pub buffers: SmallVec<[Vec<u8>; 3]>,
}

impl FrameLease {
    pub fn meta(&self) -> &FrameMeta {
        &self.meta
    }

    pub fn meta_mut(&mut self) -> &mut FrameMeta {
        &mut self.meta
    }

    pub fn is_external(&self) -> bool {
        self.external.is_some()
    }

    /// A handle that keeps this frame's external memory (a driver or device buffer) mapped and
    /// out of the driver's queue independently of the frame, for handing its pixels to an API
    /// that releases them later (e.g. an encoder that holds its input). The slices from
    /// [`FrameLease::planes`] stay valid while the handle lives. `None` for pooled host memory.
    pub fn external_backing_handle(&self) -> Option<Arc<dyn ExternalBacking>> {
        self.external.clone()
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
        // Each plane gets its own buffer holding the bytes that are there (a plane outside an
        // external backing has none), so nothing is sized from a layout's offset or length.
        let planes = self.planes();
        let max_len = planes
            .iter()
            .map(|p| p.data().len())
            .max()
            .unwrap_or(1)
            .max(1);
        let pool = BufferPool::with_limits(self.layouts.len().max(1), max_len, self.layouts.len());
        let mut layouts = SmallVec::new();
        let buffers = planes
            .into_iter()
            .zip(self.layouts.iter())
            .map(|(plane, layout)| {
                let data = plane.data();
                let mut lease = pool.lease();
                lease.resize(data.len());
                lease.as_mut_slice().copy_from_slice(data);
                layouts.push(PlaneLayout {
                    offset: 0,
                    len: data.len(),
                    stride: layout.stride,
                });
                lease
            })
            .collect();
        let mut meta = self.meta.clone();
        meta.residency = Some(FrameResidency::HostOwned);
        meta.mutability = FrameMutability::Mutable;
        let mut owned = FrameLease::multi_plane(meta, buffers, layouts);
        owned.companions = self.materialize_companions();
        owned
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
