//! Zero-copy luma (Y8) views of GREY and planar/semi-planar YUV frames.

use smallvec::smallvec;

use super::{FrameLease, FrameValidationError, VisibleRows};
use crate::format::{BitDepth, FourCc, FrameStorageKind, MediaFormat};

impl FrameLease {
    /// Whether [`FrameLease::into_luma`] can produce a GREY view without copying.
    ///
    /// True for GREY/R8 and for 8-bit planar or semi-planar YUV (NV12, NV21, NV16, I420, YV12,
    /// ...), whose first plane is a full-resolution Y8 image.
    pub fn has_luma_plane(&self) -> bool {
        luma_plane_available(self.meta.format.code)
    }

    /// Visible rows of the Y8 plane: plane 0 of GREY/R8 or planar/semi-planar YUV frames.
    ///
    /// Like [`FrameLease::visible_rows`], this works for dma-buf frames whose backing can map
    /// its planes for CPU reads (libcamera, V4L2 exports).
    pub fn luma_rows(&self) -> Result<VisibleRows<'_>, FrameValidationError> {
        if !self.has_luma_plane() {
            return Err(FrameValidationError::NoLumaPlane(self.meta.format.code));
        }
        let width = self.meta.format.resolution.width.get() as usize;
        let height = self.meta.format.resolution.height.get() as usize;
        let layout = *self.layouts.first().ok_or(FrameValidationError::NoPlanes)?;
        if layout.stride < width {
            return Err(FrameValidationError::PlaneStrideTooSmall {
                index: 0,
                stride: layout.stride,
                visible_row_bytes: width,
            });
        }
        let plane = match &self.external {
            Some(backing) => backing.plane_data(0),
            None => self.buffers.first().map(|buffer| buffer.as_slice()),
        }
        .ok_or(FrameValidationError::NoPlanes)?;
        let data = plane
            .get(layout.offset..layout.offset.saturating_add(layout.len))
            .ok_or(FrameValidationError::NoPlanes)?;
        let expected_len = layout.stride * (height - 1) + width;
        if data.len() < expected_len {
            return Err(FrameValidationError::PlaneLenTooSmall {
                index: 0,
                len: data.len(),
                expected_len,
            });
        }
        Ok(VisibleRows {
            data,
            stride: layout.stride,
            row_bytes: width,
            rows: height,
        })
    }

    /// Re-describe this frame as a GREY frame that shares the Y plane's memory.
    ///
    /// No pixels are copied: external backings (dmabuf, mmap) are shared and owned frames keep
    /// only their first buffer. Chroma planes of owned frames are released. GREY/R8 frames are
    /// returned unchanged.
    pub fn into_luma(mut self) -> Result<FrameLease, FrameValidationError> {
        let code = self.meta.format.code;
        if matches!(code, FourCc::GREY | FourCc::R8) {
            return Ok(self);
        }
        if !luma_plane_available(code) {
            return Err(FrameValidationError::NoLumaPlane(code));
        }
        let luma_layout = *self.layouts.first().ok_or(FrameValidationError::NoPlanes)?;
        if self.external.is_none() {
            if self.buffers.is_empty() {
                return Err(FrameValidationError::NoPlanes);
            }
            self.buffers.truncate(1);
        }
        self.layouts = smallvec![luma_layout];
        let format = self.meta.format;
        self.meta.format = MediaFormat::new(FourCc::GREY, format.resolution, format.color);
        Ok(self)
    }
}

fn luma_plane_available(code: FourCc) -> bool {
    if matches!(code, FourCc::GREY | FourCc::R8) {
        return true;
    }
    let info = code.layout_info();
    matches!(
        info.storage,
        FrameStorageKind::Planar | FrameStorageKind::SemiPlanar
    ) && !matches!(
        info.bit_depth,
        BitDepth::U16 | BitDepth::U16x3 | BitDepth::F32
    )
}

#[cfg(test)]
mod tests {
    use smallvec::smallvec;

    use crate::buffer::{BufferPool, FrameMeta, PlaneLayout};
    use crate::format::{ColorSpace, FourCc, MediaFormat, Resolution};

    use super::*;

    fn nv12_frame(width: u32, height: u32, stride: usize) -> FrameLease {
        let res = Resolution::new(width, height).unwrap();
        let pool = BufferPool::with_limits(2, stride * height as usize, 2);
        let y_len = stride * height as usize;
        let uv_len = stride * height as usize / 2;
        let mut y = pool.lease();
        y.resize(y_len);
        for (i, px) in y.as_mut_slice().iter_mut().enumerate() {
            *px = (i % stride) as u8;
        }
        let mut uv = pool.lease();
        uv.resize(uv_len);
        FrameLease::multi_plane(
            FrameMeta::new(MediaFormat::new(FourCc::NV12, res, ColorSpace::Bt709), 42),
            smallvec![y, uv],
            smallvec![
                PlaneLayout {
                    offset: 0,
                    len: y_len,
                    stride
                },
                PlaneLayout {
                    offset: 0,
                    len: uv_len,
                    stride
                },
            ],
        )
    }

    #[test]
    fn nv12_into_luma_keeps_stride_timestamp_and_bytes() {
        let frame = nv12_frame(6, 4, 8);
        assert!(frame.has_luma_plane());
        let luma = frame.into_luma().expect("luma view");
        assert_eq!(luma.meta().format.code, FourCc::GREY);
        assert_eq!(luma.meta().timestamp, 42);
        assert_eq!(luma.layouts().len(), 1);
        let rows = luma.luma_rows().expect("rows");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows.row(0).unwrap().data(), &[0, 1, 2, 3, 4, 5]);
        assert_eq!(luma.plane_strides().as_slice(), &[8]);
    }

    #[test]
    fn packed_formats_have_no_luma_plane() {
        let res = Resolution::new(2, 2).unwrap();
        let mut buf = BufferPool::with_limits(1, 8, 1).lease();
        buf.resize(8);
        let frame = FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(FourCc::YUYV, res, ColorSpace::Bt709), 0),
            buf,
            8,
            4,
        );
        assert!(!frame.has_luma_plane());
        assert_eq!(
            frame.into_luma().err(),
            Some(FrameValidationError::NoLumaPlane(FourCc::YUYV))
        );
    }
}
