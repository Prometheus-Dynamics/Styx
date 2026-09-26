//! Zero-copy region-of-interest views of Y8 frames.

use super::{CompanionKind, FrameLease, FrameValidationError, PlaneLayout};
use crate::format::{MediaFormat, Resolution};
use crate::requirements::FrameRect;

impl FrameLease {
    /// A view of `rect` (in this frame's pixel coordinates) sharing this frame's memory.
    ///
    /// Planar/semi-planar YUV is first reduced to its Y plane. The view keeps the source row
    /// stride; row starts are `rect.x` bytes into the source rows. Pyramid companions are
    /// cropped to the same region at their own scale. `meta().crop` records where the view sits
    /// in the full frame, composing with any earlier crop.
    pub fn crop_view(self, rect: FrameRect) -> Result<FrameLease, FrameValidationError> {
        let mut frame = self.into_luma()?;
        let width = frame.meta.format.resolution.width.get();
        let height = frame.meta.format.resolution.height.get();
        let rect = rect
            .clipped_to(width, height)
            .ok_or(FrameValidationError::ZeroDimensions)?;
        let layout = *frame
            .layouts
            .first()
            .ok_or(FrameValidationError::NoPlanes)?;
        let (x, y, w, h) = (
            rect.x as usize,
            rect.y as usize,
            rect.width as usize,
            rect.height as usize,
        );
        frame.layouts[0] = PlaneLayout {
            offset: layout.offset + y * layout.stride + x,
            len: layout.stride * (h - 1) + w,
            stride: layout.stride,
        };
        let resolution =
            Resolution::new(rect.width, rect.height).ok_or(FrameValidationError::ZeroDimensions)?;
        let format = frame.meta.format;
        frame.meta.format = MediaFormat::new(format.code, resolution, format.color);
        let origin = frame.meta.crop.map_or((0, 0), |c| (c.x, c.y));
        frame.meta.crop = Some(FrameRect::new(
            origin.0 + rect.x,
            origin.1 + rect.y,
            rect.width,
            rect.height,
        ));

        let companions = frame.take_companions();
        for (kind, companion) in companions {
            let CompanionKind::Pyramid { level } = kind;
            // A companion too small to hold any of the region is dropped.
            if let Ok(cropped) = companion.crop_view(rect.scaled_down(level)) {
                frame = frame.with_companion(kind, cropped)?;
            }
        }
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{BufferPool, FrameMeta};
    use crate::format::{ColorSpace, FourCc};

    fn grey(width: u32, height: u32) -> FrameLease {
        let len = (width * height) as usize;
        let mut buf = BufferPool::with_limits(1, len, 1).lease();
        buf.resize(len);
        for (i, px) in buf.as_mut_slice().iter_mut().enumerate() {
            *px = (i % width as usize + 10 * (i / width as usize)) as u8;
        }
        let res = Resolution::new(width, height).unwrap();
        FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), 3),
            buf,
            len,
            width as usize,
        )
    }

    #[test]
    fn crop_view_shares_memory_and_records_origin() {
        let frame = grey(8, 6).crop_view(FrameRect::new(2, 1, 4, 3)).unwrap();
        assert_eq!(frame.meta().format.resolution.width.get(), 4);
        assert_eq!(frame.plane_strides().as_slice(), &[8]);
        let rows = frame.luma_rows().unwrap();
        assert_eq!(rows.row(0).unwrap().data(), &[12, 13, 14, 15]);
        assert_eq!(rows.row(2).unwrap().data(), &[32, 33, 34, 35]);
        assert_eq!(frame.meta().crop, Some(FrameRect::new(2, 1, 4, 3)));

        let nested = frame.crop_view(FrameRect::new(1, 1, 2, 2)).unwrap();
        assert_eq!(nested.meta().crop, Some(FrameRect::new(3, 2, 2, 2)));
        assert_eq!(
            nested.luma_rows().unwrap().row(0).unwrap().data(),
            &[23, 24]
        );
    }

    #[test]
    fn crop_view_crops_pyramid_companions_at_their_scale() {
        let frame = grey(16, 8)
            .with_box_pyramid(1, 1)
            .unwrap()
            .crop_view(FrameRect::new(4, 2, 8, 4))
            .unwrap();
        let half = frame.pyramid_level(1).expect("companion kept");
        assert_eq!(half.meta().format.resolution.width.get(), 4);
        assert_eq!(half.meta().format.resolution.height.get(), 2);
        assert_eq!(half.meta().crop, Some(FrameRect::new(2, 1, 4, 2)));
    }

    #[test]
    fn empty_regions_are_rejected() {
        assert!(grey(4, 4).crop_view(FrameRect::new(8, 8, 2, 2)).is_err());
    }
}
