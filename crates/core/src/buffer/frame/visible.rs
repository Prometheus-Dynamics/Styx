//! Visible-row access, plane shapes and visible-plane copies.

#[allow(unused_imports)]
use super::*;

impl FrameLease {
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

    /// Visible rows of one plane. Dma-buf frames are readable when their backing maps the
    /// plane for CPU reads (libcamera, V4L2 exports); mapping performs any required dma-buf
    /// cache synchronisation.
    pub fn visible_rows(
        &self,
        plane_index: usize,
    ) -> Result<VisibleRows<'_>, FrameValidationError> {
        let mappable = self
            .external
            .as_ref()
            .is_some_and(|backing| backing.plane_data(plane_index).is_some());
        if !mappable {
            self.require_host_readable()?;
        }
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

    /// Allocate a host-owned frame of `format` and fill it from `bytes`: tightly packed
    /// visible rows, plane after plane in plane order. `bytes` must be exactly
    /// [`FrameLease::visible_payload_bytes`] long.
    pub fn from_visible_bytes(
        format: MediaFormat,
        timestamp: u64,
        bytes: &[u8],
    ) -> Result<Self, FrameValidationError> {
        // Check the size before allocating, so a wrong or corrupt `format` (e.g. from a damaged
        // recording) cannot allocate far more than `bytes` holds.
        let allocated: usize = super::layout::default_layouts_for_format(format, None, None)?
            .iter()
            .map(|layout| layout.offset.saturating_add(layout.len))
            .fold(0, usize::saturating_add);
        if allocated > bytes.len().saturating_mul(2).saturating_add(4096) {
            return Err(FrameValidationError::VisibleLenMismatch {
                expected: allocated,
                actual: bytes.len(),
            });
        }
        let mut frame = Self::allocate_host_owned(format, timestamp)?;
        let expected = frame.visible_payload_bytes()?;
        if bytes.len() != expected {
            return Err(FrameValidationError::VisibleLenMismatch {
                expected,
                actual: bytes.len(),
            });
        }
        let mut offset = 0;
        for plane in 0..frame.layouts.len() {
            offset += frame.copy_slice_to_visible_plane(plane, &bytes[offset..])?;
        }
        Ok(frame)
    }

    /// Copy every plane's visible rows into `dst` as tightly packed rows, plane after plane,
    /// and return the number of bytes written.
    pub fn copy_visible_to_slice(&self, dst: &mut [u8]) -> Result<usize, FrameValidationError> {
        let mut offset = 0;
        for plane in 0..self.layouts.len() {
            offset += self.copy_visible_plane_to_slice(plane, &mut dst[offset..])?;
        }
        Ok(offset)
    }

    /// Every plane's visible rows as one tightly packed buffer; the inverse of
    /// [`FrameLease::from_visible_bytes`].
    pub fn to_visible_vec(&self) -> Result<Vec<u8>, FrameValidationError> {
        let mut bytes = vec![0; self.visible_payload_bytes()?];
        self.copy_visible_to_slice(&mut bytes)?;
        Ok(bytes)
    }
}
