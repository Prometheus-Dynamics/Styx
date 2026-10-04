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
        // The view must lie within the Y plane's layout, whatever stride the frame claims (a
        // layout from another process or a recording); this also bounds the arithmetic.
        super::layout::validate_plane_layout(0, Some(&layout), width as usize, height as usize)?;
        let overflow = FrameValidationError::UnknownStorageLayout;
        let offset = y
            .checked_mul(layout.stride)
            .and_then(|o| o.checked_add(x))
            .and_then(|o| o.checked_add(layout.offset))
            .ok_or(overflow)?;
        let len = layout
            .stride
            .checked_mul(h - 1)
            .and_then(|l| l.checked_add(w))
            .ok_or(overflow)?;
        frame.layouts[0] = PlaneLayout {
            offset,
            len,
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
            let region = match kind {
                CompanionKind::Pyramid { level } => rect.scaled_down(level),
                CompanionKind::Scaled => {
                    let (from, to) = (format.resolution, companion.meta.format.resolution);
                    rect.scaled(
                        (from.width.get(), from.height.get()),
                        (to.width.get(), to.height.get()),
                    )
                }
            };
            // A companion too small to hold any of the region is dropped.
            if let Ok(cropped) = companion.crop_view(region) {
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

    #[test]
    fn crops_of_frames_with_absurd_strides_are_refused() {
        // Found by the `core_frame_layout` fuzz target: a 60288x65535 frame claiming a
        // 9.8e18-byte stride over a 29 KB plane overflowed computing the view.
        let res = Resolution::new(60288, 65535).unwrap();
        let mut buf = BufferPool::with_limits(1, 4351 + 29041, 0).lease();
        buf.resize(4351 + 29041);
        let layout = PlaneLayout {
            offset: 4351,
            len: 29041,
            stride: 9_813_625_275_759_559_555,
        };
        let frame = FrameLease::multi_plane(
            FrameMeta::new(MediaFormat::new(FourCc::R8, res, ColorSpace::Srgb), 0),
            smallvec::smallvec![buf],
            smallvec::smallvec![layout],
        );
        assert!(
            frame
                .crop_view(FrameRect::new(19306, 106, 31375, 378))
                .is_err()
        );
    }
}
