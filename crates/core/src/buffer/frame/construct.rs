//! FrameLease constructors and host allocation.

#[allow(unused_imports)]
use super::*;

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
            companions: None,
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
            companions: None,
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
            companions: None,
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
            companions: None,
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
}
