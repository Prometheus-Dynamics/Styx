//! A consumer's frames from a capture's: its ISP output (the frame, its second output or one
//! of its region companions), its other regions renumbered as it asked for them, views of the
//! regions the ISP does not crop, the overview box-filtered on the CPU where no ISP makes it,
//! and whether a frame's crops are current ([`FrameRequest::skip_stale_regions`]).
//!
//! [`FrameRequest::skip_stale_regions`]: super::FrameRequest::skip_stale_regions

use smallvec::smallvec;
use styx_codec::CodecError;
use styx_core::prelude::*;

use super::FramePlan;
use super::region::{IspPlace, RoiCrop};
use super::roi::RoiHandle;

fn codec_err(e: impl std::fmt::Display) -> CodecError {
    CodecError::Codec(e.to_string())
}

/// `frame` as `plan`'s consumer gets it from the capture: its second output (a consumer on
/// it), or its region 0 (the main output, or the region companion its slot makes) with its
/// other regions as `CompanionKind::Region { index }` in its own numbering, the overview, and
/// the ISP's pyramid levels when they are of its frame. Other consumers' outputs are dropped.
pub(crate) fn consumer_frame(
    mut frame: FrameLease,
    plan: &FramePlan,
) -> Result<FrameLease, CodecError> {
    let companions = frame.take_companions();
    if plan.isp_second_output {
        return companions
            .into_iter()
            .find(|(kind, _)| *kind == CompanionKind::Scaled)
            .map(|(_, frame)| frame)
            .ok_or_else(|| codec_err("frame without the ISP's second output"));
    }
    let places = &plan.region.places;
    let index_of = |slot: u8| places.iter().position(|p| *p == Some(IspPlace::Slot(slot)));
    let mut companions: Vec<_> = companions.into_iter().collect();
    let (mut primary, main_is_primary) = match places.first() {
        Some(Some(IspPlace::Slot(k))) => {
            let slot = CompanionKind::Region { index: k + 1 };
            // A region its pass did not make this frame (unset, or no free buffer): skipped.
            let at = companions
                .iter()
                .position(|(kind, _)| *kind == slot)
                .ok_or_else(|| codec_err("frame without its region"))?;
            (companions.swap_remove(at).1, false)
        }
        _ => (frame, true),
    };
    for (kind, companion) in companions {
        let kind = match kind {
            CompanionKind::Region { index } => match index_of(index - 1) {
                Some(i) if i > 0 => CompanionKind::Region { index: i as u8 },
                _ => continue,
            },
            CompanionKind::Overview if plan.region.overview.is_some() => kind,
            CompanionKind::Pyramid { .. } if main_is_primary => kind,
            _ => continue,
        };
        primary = primary.with_companion(kind, companion).map_err(codec_err)?;
    }
    Ok(primary)
}

/// Whether `frame` (as [`consumer_frame`] made it) shows the regions `roi` holds now, in the
/// crops the ISP made: every ISP-cropped region must lie inside its frame's crop (`None` set:
/// the main output whole, or no region). Views are always current.
pub(crate) fn fresh(frame: &FrameLease, plan: &FramePlan, roi: &RoiHandle) -> bool {
    if !plan.request.skip_stale_regions {
        return true;
    }
    for (i, crop) in plan.region.crops.iter().enumerate() {
        if !matches!(crop, Some(RoiCrop::Isp | RoiCrop::IspPass)) {
            continue;
        }
        let want = roi.region(i);
        let got = frame.region(i as u8).and_then(|f| f.meta().crop);
        let ok = match (want, got) {
            (None, None) => true,
            (Some(w), Some(g)) => covers(g, w),
            // A region set but not made yet, or the whole frame wanted while a crop came.
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// `outer` contains `inner` (as the ISP rounds it: out to even pixels, at least 16x16, so
/// clipped to what `outer` could hold).
fn covers(outer: FrameRect, inner: FrameRect) -> bool {
    let (x, y) = (inner.x & !1, inner.y & !1);
    let (x1, y1) = (inner.x + inner.width, inner.y + inner.height);
    outer.x <= x && outer.y <= y && outer.x + outer.width >= x1 && outer.y + outer.height >= y1
}

/// `frame` with views of `views` (`(index, rect)`, rects in `frame`'s pixels) attached as its
/// region companions: zero-copy crops of the frame (luma frames).
pub(crate) fn attach_views(
    frame: FrameLease,
    views: &[(u8, FrameRect)],
) -> Result<FrameLease, CodecError> {
    if views.is_empty() {
        return Ok(frame);
    }
    let mut frame = frame.into_shareable();
    for &(index, rect) in views {
        let Some(mut share) = frame.share() else {
            break;
        };
        share.take_companions();
        let view = share.crop_view(rect).map_err(codec_err)?;
        frame = frame
            .with_companion(CompanionKind::Region { index }, view)
            .map_err(codec_err)?;
    }
    Ok(frame)
}

/// The whole of `frame`, its luma box-filtered down to `size` (halved while the half still
/// covers it, then an area resize), as a GREY overview; `None` for frames without a luma plane.
pub(crate) fn box_overview(
    frame: &FrameLease,
    size: (u32, u32),
    pool: &BufferPool,
) -> Result<Option<FrameLease>, CodecError> {
    if !frame.has_luma_plane() {
        return Ok(None);
    }
    let mut level: Option<FrameLease> = None;
    loop {
        let source = level.as_ref().unwrap_or(frame);
        let res = source.meta().format.resolution;
        let (w, h) = (res.width.get(), res.height.get());
        if w / 2 < size.0 || h / 2 < size.1 {
            break;
        }
        let half = box_downscale_luma_in(source, 64, pool).map_err(codec_err)?;
        level = Some(half);
    }
    let source = level.as_ref().unwrap_or(frame);
    let res = source.meta().format.resolution;
    let mut out = if (res.width.get(), res.height.get()) == size {
        match level {
            Some(l) => l,
            None => {
                let mut l = frame
                    .share()
                    .ok_or_else(|| codec_err("frame not shareable"))?;
                l.take_companions();
                l.into_luma().map_err(codec_err)?
            }
        }
    } else {
        area_resize_luma(source, size, pool)?
    };
    out.meta_mut().crop = None;
    Ok(Some(out))
}

/// `source`'s luma resized down to `size` by area averaging (each output pixel the mean of the
/// source pixels it covers, fractions weighted), into a GREY frame from `pool`.
fn area_resize_luma(
    source: &FrameLease,
    (w, h): (u32, u32),
    pool: &BufferPool,
) -> Result<FrameLease, CodecError> {
    let rows = source.luma_rows().map_err(codec_err)?;
    let (sw, sh) = (rows.row_bytes(), rows.len());
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || w > sw || h > sh {
        return Err(codec_err("overview larger than the frame"));
    }
    // Weights in 1/256ths of a source pixel per output pixel, along one axis.
    let taps = |from: usize, to: usize| -> Vec<Vec<(usize, u32)>> {
        (0..to)
            .map(|o| {
                let (a, b) = (o * from * 256 / to, (o + 1) * from * 256 / to);
                let mut t = Vec::new();
                let mut p = a;
                while p < b {
                    let next = ((p / 256) + 1) * 256;
                    let end = next.min(b);
                    t.push((p / 256, (end - p) as u32));
                    p = end;
                }
                t
            })
            .collect()
    };
    let (tx, ty) = (taps(sw, w), taps(sh, h));
    let total = |t: &[(usize, u32)]| t.iter().map(|&(_, f)| u64::from(f)).sum::<u64>();
    let tx_total: Vec<u64> = tx.iter().map(|t| total(t)).collect();
    let stride = w.next_multiple_of(64);
    let mut buf = pool.lease();
    buf.resize(stride * h);
    let mut acc = vec![0u64; w];
    for (oy, wy) in ty.iter().enumerate() {
        acc.fill(0);
        for &(sy, fy) in wy {
            let Some(row) = rows.row(sy) else { continue };
            let row = row.data();
            for (a, wx) in acc.iter_mut().zip(&tx) {
                let s: u64 = wx
                    .iter()
                    .map(|&(sx, fx)| u64::from(row[sx]) * u64::from(fx))
                    .sum();
                *a += s * u64::from(fy);
            }
        }
        let fy_total = total(wy);
        let out = &mut buf.as_mut_slice()[oy * stride..oy * stride + w];
        for ((o, a), tx) in out.iter_mut().zip(&acc).zip(&tx_total) {
            let area = (tx * fy_total).max(1);
            *o = ((a + area / 2) / area).min(255) as u8;
        }
    }
    let resolution =
        Resolution::new(w as u32, h as u32).ok_or_else(|| codec_err("empty overview"))?;
    let format = MediaFormat::new(FourCc::GREY, resolution, source.meta().format.color);
    let mut meta = FrameMeta::new(format, source.meta().timestamp);
    meta.backend = source.meta().backend.clone();
    meta.capture_instant = source.meta().capture_instant;
    meta.clock = source.meta().clock;
    meta.hops = source.meta().hops;
    // New pixels, scaled from the frame's.
    styx_core::metrics::copied_frame(&mut meta, styx_core::metrics::CopySite::Other, stride * h);
    Ok(FrameLease::multi_plane(
        meta,
        smallvec![buf],
        smallvec![PlaneLayout {
            offset: 0,
            len: stride * h,
            stride,
        }],
    ))
}
