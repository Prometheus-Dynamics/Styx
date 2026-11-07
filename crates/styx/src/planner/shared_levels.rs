//! CPU pyramid levels of an exportable plan written straight into memfd buffers, so a camera
//! service hands its clients the fd instead of copying each level out of the heap.

use styx_codec::CodecError;
use styx_core::buffer::SharedBufferPool;
use styx_core::prelude::*;

/// `frame` with box-filtered levels `1..=levels` (2x2, each from the one before; levels already
/// attached, e.g. by an ISP, are kept), each in a buffer from `pool` with rows at multiples of
/// `align` (a memfd mapping is page aligned).
pub(super) fn with_shared_box_pyramid(
    frame: FrameLease,
    levels: u8,
    align: usize,
    pool: &SharedBufferPool,
) -> Result<FrameLease, CodecError> {
    let err = |e: &dyn std::fmt::Display| CodecError::Codec(e.to_string());
    let mut frame = frame;
    for level in 1..=levels {
        if frame.pyramid_level(level).is_some() {
            continue;
        }
        let next = {
            let source = frame
                .pyramid_level(level - 1)
                .ok_or_else(|| CodecError::Codec("no source level".into()))?;
            box_level(source, level, align, pool)?
        };
        frame = frame
            .with_companion(CompanionKind::Pyramid { level }, next)
            .map_err(|e| err(&e))?;
    }
    Ok(frame)
}

fn box_level(
    source: &FrameLease,
    level: u8,
    align: usize,
    pool: &SharedBufferPool,
) -> Result<FrameLease, CodecError> {
    let err = |e: &dyn std::fmt::Display| CodecError::Codec(e.to_string());
    let rows = source.luma_rows().map_err(|e| err(&e))?;
    let (width, height) = (rows.row_bytes() / 2, rows.len() / 2);
    let resolution = Resolution::new(width as u32, height as u32)
        .ok_or_else(|| CodecError::Codec("an empty pyramid level".into()))?;
    let stride = width.next_multiple_of(align.max(1));
    let len = stride * height;
    let mut lease = pool
        .lease()
        .and_then(|mut lease| lease.try_resize(len).map(|()| lease))
        .map_err(|e| err(&e))?;
    styx_core::metrics::moved(
        styx_core::metrics::TrafficStage::CpuPyramid,
        (4 * width * height) as u64,
        (width * height) as u64,
    );
    let dst = &mut lease.as_mut_slice()[..len];
    for (y, out) in dst.chunks_exact_mut(stride).enumerate() {
        if let (Some(top), Some(bottom)) = (rows.row(2 * y), rows.row(2 * y + 1)) {
            styx_core::simd::box2_row(top.data(), bottom.data(), &mut out[..width], width);
        }
    }
    let meta = level_meta(source, level, resolution);
    FrameLease::single_plane_shared(meta, lease, len, stride).map_err(|e| err(&e))
}
