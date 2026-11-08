//! Companion frames: lower-resolution views of the same instant attached to a frame.
//!
//! A detector can search the ½ or ¼ companion for candidates and refine on the full-resolution
//! primary. Producers guarantee the companion shows the same capture (same timestamp): an ISP's
//! second output from the same request, or a filter applied to the decoded frame.

use alloc::boxed::Box;
use smallvec::{SmallVec, smallvec};

use super::{FrameLease, FrameValidationError, PlaneLayout};
use crate::buffer::{BufferPool, FrameMeta};
use crate::format::{FourCc, MediaFormat, Resolution};

// Four inline: three pyramid levels and one more (an overview, a raw frame) without a spill.
pub(super) type Companions = SmallVec<[(CompanionKind, FrameLease); 4]>;

/// Empty companion lists for reuse: a frame with companions then allocates none per frame.
#[cfg(feature = "std")]
#[allow(clippy::vec_box)] // The boxes themselves are what frames reuse.
static FREE_LISTS: crate::sync::Mutex<alloc::vec::Vec<Box<Companions>>> =
    crate::sync::Mutex::new(alloc::vec::Vec::new());

/// At most this many lists wait for reuse (the frames with companions in flight).
#[cfg(feature = "std")]
const KEEP_LISTS: usize = 32;

fn take_list() -> Box<Companions> {
    #[cfg(feature = "std")]
    if let Some(list) = FREE_LISTS.lock().pop() {
        return list;
    }
    Box::default()
}

/// A frame's companions are going: they drop, and their list is kept for another frame.
#[cfg(feature = "std")]
pub(super) fn recycle(list: Option<Box<Companions>>) {
    let Some(mut list) = list else {
        return;
    };
    list.clear();
    let mut free = FREE_LISTS.lock();
    if free.capacity() == 0 {
        free.reserve_exact(KEEP_LISTS);
    }
    if free.len() < KEEP_LISTS {
        free.push(list);
    }
}

/// Companions taken off a frame ([`FrameLease::take_companions`]), in the order they were
/// attached. When this is dropped the list goes back for another frame, so taking companions
/// and attaching them again allocates nothing per frame.
#[derive(Default)]
pub struct TakenCompanions {
    /// Reversed: the next companion is at the end.
    list: Option<Box<Companions>>,
}

impl TakenCompanions {
    /// Takes the companion of `kind` out of those left, if it is there.
    pub fn remove(&mut self, kind: CompanionKind) -> Option<FrameLease> {
        let list = self.list.as_mut()?;
        let at = list.iter().position(|(existing, _)| *existing == kind)?;
        Some(list.remove(at).1)
    }
}

impl core::fmt::Debug for TakenCompanions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TakenCompanions")
            .field("len", &self.len())
            .finish()
    }
}

impl Iterator for TakenCompanions {
    type Item = (CompanionKind, FrameLease);

    fn next(&mut self) -> Option<Self::Item> {
        self.list.as_mut()?.pop()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.list.as_ref().map_or(0, |list| list.len());
        (len, Some(len))
    }
}

impl ExactSizeIterator for TakenCompanions {}

#[cfg(feature = "std")]
impl Drop for TakenCompanions {
    fn drop(&mut self) {
        recycle(self.list.take());
    }
}

/// How a companion relates to its primary frame.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CompanionKind {
    /// Downscaled by `2^level` in each axis (level 1 = ½, 2 = ¼, 3 = ⅛).
    Pyramid { level: u8 },
    /// The same capture at another size, from its own output (a Raspberry Pi ISP's second
    /// output), for a consumer that wants that size.
    Scaled,
    /// The whole frame, scaled, when the primary frame is a region of it (an ISP crop or a
    /// crop view; `meta().crop` says where): a low-resolution view to find the next region
    /// in. Crops leave it alone.
    Overview,
    /// Another region of interest of the same capture (`index` 1, 2, ...; the primary frame
    /// is region 0), cropped by the ISP from the same raw frame: `meta().crop` says where.
    /// Crops leave it alone.
    Region { index: u8 },
    /// The raw sensor frame the primary was processed from (a native camera's ISP input, as
    /// the ISP read it), for a consumer that records or processes raw frames.
    SensorRaw,
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
        let companions = self.companions.get_or_insert_with(take_list);
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

    /// The whole frame scaled down, when this frame is a region of it
    /// ([`CompanionKind::Overview`]).
    pub fn overview(&self) -> Option<&FrameLease> {
        self.companion(CompanionKind::Overview)
    }

    /// Region of interest `index` of this capture: 0 is this frame, 1, 2, ... the
    /// [`CompanionKind::Region`] companions.
    pub fn region(&self, index: u8) -> Option<&FrameLease> {
        if index == 0 {
            return Some(self);
        }
        self.companion(CompanionKind::Region { index })
    }

    /// This frame and its region companions, by region index (see [`Self::region`]).
    pub fn regions(&self) -> impl Iterator<Item = (u8, &FrameLease)> {
        core::iter::once((0, self)).chain(self.companions().filter_map(|(kind, f)| match kind {
            CompanionKind::Region { index } => Some((index, f)),
            _ => None,
        }))
    }

    /// All attached companions.
    pub fn companions(&self) -> impl Iterator<Item = (CompanionKind, &FrameLease)> {
        self.companions
            .iter()
            .flat_map(|companions| companions.iter().map(|(kind, frame)| (*kind, frame)))
    }

    /// Detach and return all companions, e.g. to process them separately.
    pub fn take_companions(&mut self) -> TakenCompanions {
        let mut list = self.companions.take();
        if let Some(list) = &mut list {
            list.reverse();
        }
        TakenCompanions { list }
    }

    /// Attach GREY pyramid levels `1..=levels` computed from this frame's Y plane with 2×2 box
    /// filters, each level from the previous one. Levels a hardware producer already attached
    /// are kept and reused as the source for deeper levels.
    ///
    /// Costs ~0.09 ms for a 1280x800 → 640x400 level from a frame out of cache on a Cortex-A76
    /// (CM5); ~0.14 ms before the NEON loop prefetched its source rows.
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
    let rows = source.luma_rows()?;
    let (width, height) = (rows.row_bytes() / 2, rows.len() / 2);
    #[cfg(feature = "path-metrics")]
    crate::metrics::moved(
        crate::metrics::TrafficStage::CpuPyramid,
        (4 * width * height) as u64,
        (width * height) as u64,
    );
    luma_level_in(
        source,
        1,
        width,
        height,
        stride_alignment,
        pool,
        |y, out| {
            if let (Some(top), Some(bottom)) = (rows.row(2 * y), rows.row(2 * y + 1)) {
                crate::simd::box2_row(top.data(), bottom.data(), out, width);
            }
        },
    )
}

/// A GREY frame for pyramid level `level` of `source` (`width` x `height`, rows aligned to
/// `stride_alignment`, in a buffer from `pool`), each row written by `fill(y, row)`: the
/// metadata of `source` with its crop scaled down. For producers of levels made elsewhere
/// (e.g. on a GPU) that attach them as [`CompanionKind::Pyramid`].
pub fn luma_level_in(
    source: &FrameLease,
    level: u8,
    width: usize,
    height: usize,
    stride_alignment: usize,
    pool: &BufferPool,
    mut fill: impl FnMut(usize, &mut [u8]),
) -> Result<FrameLease, FrameValidationError> {
    if !stride_alignment.is_power_of_two() {
        return Err(FrameValidationError::InvalidAlignment(stride_alignment));
    }
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
        fill(y, &mut out[..width]);
    }

    let meta = level_meta(source, level, resolution);
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

/// The metadata of `source`'s pyramid level `level` at `resolution`: GREY, the source's
/// timestamp, backend and capture instant, its crop scaled down. For producers that write a
/// level into memory of their own (e.g. a memfd to serve it).
pub fn level_meta(source: &FrameLease, level: u8, resolution: Resolution) -> FrameMeta {
    let format = MediaFormat::new(FourCc::GREY, resolution, source.meta.format.color);
    let mut meta = FrameMeta::new(format, source.meta.timestamp);
    meta.backend = source.meta.backend.clone();
    meta.capture_instant = source.meta.capture_instant;
    meta.crop = source.meta.crop.map(|crop| crop.scaled_down(level));
    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::ColorSpace;
    use alloc::vec::Vec;

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
    fn taken_companions_keep_their_order_and_one_can_be_removed() {
        let frame = |ts| grey(4, 4, 4, ts, |_, _| 0);
        let level = |level| CompanionKind::Pyramid { level };
        let mut primary = frame(1);
        for l in 1..=5 {
            // Five: past the inline capacity as well.
            primary = primary.with_companion(level(l), frame(1)).unwrap();
        }
        let mut taken = primary.take_companions();
        assert_eq!(taken.len(), 5);
        assert!(taken.remove(level(3)).is_some());
        assert!(taken.remove(level(3)).is_none());
        let order: Vec<_> = taken.map(|(kind, _)| kind).collect();
        assert_eq!(order, [level(1), level(2), level(4), level(5)]);
        assert!(primary.companions().next().is_none());
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

    /// The rule consumers check levels against (e.g. Eidos's `is_box_reduction`): each pixel
    /// `(a + b + c + d + 2) / 4` of its 2x2 block, `floor(w / 2) x floor(h / 2)`, an odd last
    /// row or column ignored. Odd and even sizes, every level, every value (SIMD rows included).
    #[test]
    fn box_levels_round_half_up_and_drop_odd_edges() {
        for (w, h) in [(17u32, 9u32), (33, 21), (64, 48), (1281, 801)] {
            let frame = grey(w, h, w.next_multiple_of(64) as usize, 0, |x, y| {
                (x.wrapping_mul(37) ^ y.wrapping_mul(101)) as u8
            })
            .with_box_pyramid(3, 64)
            .expect("pyramid");
            for level in 1..=3u8 {
                let src = frame.pyramid_level(level - 1).unwrap().luma_rows().unwrap();
                let dst = frame.pyramid_level(level).unwrap();
                let (sw, sh) = (src.row_bytes(), src.len());
                let res = dst.meta().format.resolution;
                assert_eq!(
                    (res.width.get() as usize, res.height.get() as usize),
                    (sw / 2, sh / 2)
                );
                let dst = dst.luma_rows().unwrap();
                for y in 0..sh / 2 {
                    let (t, b) = (src.row(2 * y).unwrap(), src.row(2 * y + 1).unwrap());
                    let (t, b, d) = (t.data(), b.data(), dst.row(y).unwrap());
                    for x in 0..sw / 2 {
                        let sum: u32 = [t[2 * x], t[2 * x + 1], b[2 * x], b[2 * x + 1]]
                            .iter()
                            .map(|&v| u32::from(v))
                            .sum();
                        assert_eq!(
                            u32::from(d.data()[x]),
                            (sum + 2) / 4,
                            "{w}x{h} L{level} ({x},{y})"
                        );
                    }
                }
            }
        }
    }
}
