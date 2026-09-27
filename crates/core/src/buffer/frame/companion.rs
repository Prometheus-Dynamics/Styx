//! Companion frames: lower-resolution views of the same instant attached to a frame.
//!
//! A detector can search the ½ or ¼ companion for candidates and refine on the full-resolution
//! primary. Producers guarantee the companion shows the same capture (same timestamp): an ISP's
//! second output from the same request, or a filter applied to the decoded frame.

use smallvec::{SmallVec, smallvec};

use super::{FrameLease, FrameValidationError, PlaneLayout};
use crate::buffer::{BufferPool, FrameMeta};
use crate::format::{FourCc, MediaFormat, Resolution};

pub(super) type Companions = SmallVec<[(CompanionKind, FrameLease); 2]>;

/// How a companion relates to its primary frame.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CompanionKind {
    /// Downscaled by `2^level` in each axis (level 1 = ½, 2 = ¼, 3 = ⅛).
    Pyramid { level: u8 },
}

impl FrameLease {
    /// Attach a companion. It must share this frame's timestamp and have no companions itself.
    /// An existing companion of the same kind is replaced.
    pub fn with_companion(
        mut self,
        kind: CompanionKind,
        companion: FrameLease,
    ) -> Result<Self, FrameValidationError> {
        if companion.meta.timestamp != self.meta.timestamp {
            return Err(FrameValidationError::CompanionTimestampMismatch {
                frame: self.meta.timestamp,
                companion: companion.meta.timestamp,
            });
        }
        if companion.companions.is_some() {
            return Err(FrameValidationError::NestedCompanion);
        }
        let companions = self.companions.get_or_insert_with(Default::default);
        companions.retain(|(existing, _)| *existing != kind);
        companions.push((kind, companion));
        Ok(self)
    }

    /// The companion of `kind`, if one is attached.
    pub fn companion(&self, kind: CompanionKind) -> Option<&FrameLease> {
        self.companions
            .as_ref()?
            .iter()
            .find(|(existing, _)| *existing == kind)
            .map(|(_, frame)| frame)
    }

    /// Pyramid level `level`: 0 is this frame, 1 the ½ companion, 2 the ¼ companion, ...
    pub fn pyramid_level(&self, level: u8) -> Option<&FrameLease> {
        if level == 0 {
            return Some(self);
        }
        self.companion(CompanionKind::Pyramid { level })
    }

    /// All attached companions.
    pub fn companions(&self) -> impl Iterator<Item = (CompanionKind, &FrameLease)> {
        self.companions
            .iter()
            .flat_map(|companions| companions.iter().map(|(kind, frame)| (*kind, frame)))
    }

    /// Detach and return all companions, e.g. to process them separately.
    pub fn take_companions(&mut self) -> Vec<(CompanionKind, FrameLease)> {
        self.companions
            .take()
            .map(|companions| companions.into_vec())
            .unwrap_or_default()
    }

    /// Attach GREY pyramid levels `1..=levels` computed from this frame's Y plane with 2×2 box
    /// filters, each level from the previous one. Levels a hardware producer already attached
    /// are kept and reused as the source for deeper levels.
    ///
    /// Costs ~0.16 ms for a 1280x800 → 640x400 level on a Cortex-A76 (CM5).
    pub fn with_box_pyramid(
        self,
        levels: u8,
        stride_alignment: usize,
    ) -> Result<Self, FrameValidationError> {
        let pool = BufferPool::lazy(0, levels as usize);
        self.with_box_pyramid_in(levels, stride_alignment, &pool)
    }

    /// [`FrameLease::with_box_pyramid`] drawing level buffers from `pool`, so steady-state
    /// capture recycles them instead of allocating per frame.
    pub fn with_box_pyramid_in(
        self,
        levels: u8,
        stride_alignment: usize,
        pool: &BufferPool,
    ) -> Result<Self, FrameValidationError> {
        let mut frame = self;
        for level in 1..=levels {
            if frame.pyramid_level(level).is_some() {
                continue;
            }
            let source = frame
                .pyramid_level(level - 1)
                .ok_or(FrameValidationError::NoPlanes)?;
            let next = box_downscale_luma_in(source, stride_alignment, pool)?;
            frame = frame.with_companion(CompanionKind::Pyramid { level }, next)?;
        }
        Ok(frame)
    }

    pub(super) fn materialize_companions(&self) -> Option<Box<Companions>> {
        let companions = self.companions.as_ref()?;
        Some(Box::new(
            companions
                .iter()
                .map(|(kind, frame)| (*kind, frame.materialize_owned()))
                .collect(),
        ))
    }
}

/// Halve a frame's Y plane with a rounded 2×2 box filter into a new GREY frame.
pub fn box_downscale_luma(
    source: &FrameLease,
    stride_alignment: usize,
) -> Result<FrameLease, FrameValidationError> {
    box_downscale_luma_in(source, stride_alignment, &BufferPool::lazy(0, 1))
}

/// [`box_downscale_luma`] writing into a buffer leased from `pool`.
pub fn box_downscale_luma_in(
    source: &FrameLease,
    stride_alignment: usize,
    pool: &BufferPool,
) -> Result<FrameLease, FrameValidationError> {
    if !stride_alignment.is_power_of_two() {
        return Err(FrameValidationError::InvalidAlignment(stride_alignment));
    }
    let rows = source.luma_rows()?;
    let width = rows.row_bytes() / 2;
    let height = rows.len() / 2;
    let resolution =
        Resolution::new(width as u32, height as u32).ok_or(FrameValidationError::ZeroDimensions)?;
    let stride = width.next_multiple_of(stride_alignment);
    let len = stride * height;

    let mut buf = pool.lease();
    buf.resize(len + stride_alignment - 1);
    let base = buf.as_slice().as_ptr() as usize;
    let offset = base.next_multiple_of(stride_alignment) - base;
    let dst = &mut buf.as_mut_slice()[offset..offset + len];
    for (y, out) in dst.chunks_exact_mut(stride).enumerate() {
        let (Some(top), Some(bottom)) = (rows.row(2 * y), rows.row(2 * y + 1)) else {
            break;
        };
        crate::simd::box2_row(top.data(), bottom.data(), &mut out[..width], width);
    }

    let format = MediaFormat::new(FourCc::GREY, resolution, source.meta.format.color);
    let mut meta = FrameMeta::new(format, source.meta.timestamp);
    meta.backend = source.meta.backend.clone();
    meta.capture_instant = source.meta.capture_instant;
    meta.crop = source.meta.crop.map(|crop| crop.scaled_down(1));
    Ok(FrameLease::multi_plane(
        meta,
        smallvec![buf],
        smallvec![PlaneLayout {
            offset,
            len,
            stride,
        }],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::ColorSpace;

    fn grey(
        width: u32,
        height: u32,
        stride: usize,
        ts: u64,
        fill: impl Fn(usize, usize) -> u8,
    ) -> FrameLease {
        let mut buf = BufferPool::with_limits(1, stride * height as usize, 1).lease();
        buf.resize(stride * height as usize);
        for y in 0..height as usize {
            for x in 0..width as usize {
                buf.as_mut_slice()[y * stride + x] = fill(x, y);
            }
        }
        let res = Resolution::new(width, height).unwrap();
        FrameLease::single_plane(
            FrameMeta::new(MediaFormat::new(FourCc::GREY, res, ColorSpace::Unknown), ts),
            buf,
            stride * height as usize,
            stride,
        )
    }

    #[test]
    fn box_pyramid_levels_average_and_align() {
        let frame = grey(8, 4, 8, 7, |x, y| (x * 10 + y) as u8)
            .with_box_pyramid(2, 64)
            .expect("pyramid");
        let half = frame.pyramid_level(1).expect("½");
        assert_eq!(half.meta().format.resolution.width.get(), 4);
        assert_eq!(half.meta().timestamp, 7);
        assert_eq!(half.plane_strides().as_slice(), &[64]);
        assert_eq!(half.planes()[0].data().as_ptr() as usize % 64, 0);
        // (0 + 10 + 1 + 11 + 2) >> 2 = 6
        assert_eq!(half.luma_rows().unwrap().row(0).unwrap().data()[0], 6);
        let quarter = frame.pyramid_level(2).expect("¼");
        assert_eq!(quarter.meta().format.resolution.width.get(), 2);
        assert_eq!(quarter.meta().format.resolution.height.get(), 1);
        assert_eq!(frame.companions().count(), 2);
    }

    #[test]
    fn companions_must_match_timestamp_and_not_nest() {
        let small = grey(2, 2, 2, 2, |_, _| 0);
        let err = grey(4, 4, 4, 1, |_, _| 0)
            .with_companion(CompanionKind::Pyramid { level: 1 }, small)
            .err();
        assert_eq!(
            err,
            Some(FrameValidationError::CompanionTimestampMismatch {
                frame: 1,
                companion: 2
            })
        );

        let nested = grey(2, 2, 2, 1, |_, _| 0)
            .with_box_pyramid(1, 1)
            .expect("pyramid");
        let err = grey(4, 4, 4, 1, |_, _| 0)
            .with_companion(CompanionKind::Pyramid { level: 1 }, nested)
            .err();
        assert_eq!(err, Some(FrameValidationError::NestedCompanion));
    }

    #[test]
    fn pooled_pyramid_recycles_level_buffers() {
        let pool = BufferPool::lazy(0, 4);
        for _ in 0..3 {
            let frame = grey(64, 32, 64, 1, |x, y| (x + y) as u8)
                .with_box_pyramid_in(2, 64, &pool)
                .unwrap();
            assert!(frame.pyramid_level(2).is_some());
        }
        // Two levels leased per frame; later frames reuse the returned buffers.
        assert!(pool.metrics().hits() >= 2);
    }

    #[test]
    fn materialize_keeps_companions() {
        let frame = grey(4, 4, 4, 3, |x, _| x as u8)
            .with_box_pyramid(1, 1)
            .unwrap();
        let owned = frame.materialize_owned();
        assert!(owned.pyramid_level(1).is_some());
    }
}
