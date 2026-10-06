//! Running a [`FramePlan`]: capture, then a single preparation stage that applies the planned
//! route (decode or luma view), region of interest, pyramid levels and row alignment.

use std::sync::Arc;

#[cfg(feature = "codec-turbojpeg")]
use parking_lot::Mutex;
use smallvec::smallvec;
#[cfg(feature = "codec-turbojpeg")]
use styx_codec::prelude::{
    LumaCrop, LumaDecodeOptions, LumaDecodeScale, TurbojpegDecoder, TurbojpegLumaDecoder,
};
use styx_codec::{Codec, CodecDescriptor, CodecError, CodecKind};
use styx_core::prelude::*;

pub use super::frames::Frames;
#[allow(deprecated)]
pub use super::frames::PlannedFrames;
pub use super::roi::RoiHandle;
use super::{FramePlan, Route};
use crate::capture_api::{CaptureError, CaptureRequest, IdleStop};
use crate::session::MediaPipelineBuilder;

impl FramePlan {
    /// Start capturing with this plan.
    pub fn start(&self) -> Result<Frames, CaptureError> {
        let mut config = self
            .config
            .clone()
            .unwrap_or_default()
            .capture_queue_depth(self.queue_depth);
        if let Some(level) = self.isp_pyramid_level {
            config = config
                .libcamera_pyramid_level(level)
                .native_pyramid_level(level);
        }
        if let Some((width, height)) = self.isp_output {
            config = config
                .libcamera_output_size(width, height)
                .native_output_size(width, height);
        }
        config = self.region_config(config);
        config = match self.stop_when_idle {
            Some((after, IdleStop::Pause)) => config.pause_when_idle(after),
            Some((after, _)) => config.stop_when_idle(after),
            None => config,
        };
        let roi = RoiHandle::default();
        roi.set_regions(&self.request.all_regions());
        let preparer = Arc::new(FramePreparer::new(self, roi.clone()));
        #[cfg(target_os = "linux")]
        if let Some(buffers) = &self.capture_buffers
            && preparer.passes_through()
        {
            config = config.capture_into(buffers.clone());
        }
        let mut capture = CaptureRequest::new(&self.device)
            .backend(self.backend)
            .mode(self.mode.id.clone())
            .config(config);
        if let Some(interval) = self.interval {
            capture = capture.interval(interval);
        }
        let builder = MediaPipelineBuilder::new(capture).decoder(preparer.clone());
        // Planned frames are for an in-process consumer: zero-copy views and pooled buffers,
        // not memfd/dma-buf exports for other processes.
        #[cfg(target_os = "linux")]
        let builder = builder.shared_decode_output(false);
        let pipeline = builder.start()?;
        if self.region.isp() {
            roi.crop_in_isp(pipeline.capture(), self.region.places.clone());
        }
        Ok(Frames::of_pipeline(pipeline, roi, self.clone(), preparer))
    }
}

/// One pipeline stage carrying out everything after capture.
pub(crate) struct FramePreparer {
    descriptor: CodecDescriptor,
    route: Route,
    luma: bool,
    /// The ISP crops region 0 (main output or a region slot): frames arrive cropped.
    isp_crop: bool,
    /// The ISP makes regions or the overview: frames arrive with them as companions, numbered
    /// by the capture (`region_frames::consumer_frame` makes them this consumer's).
    isp_regions: Option<Box<FramePlan>>,
    /// The overview made here (not by the ISP), at this size: box-filtered from the frame's
    /// luma, or the uncropped frame itself when it is that size.
    view_overview: Option<(u32, u32)>,
    /// Regions after the first made here, as views (their indices).
    view_regions: Vec<u8>,
    pyramid_levels: u8,
    alignment: Option<usize>,
    /// Frames are decoded at 1/`decode_scale` size.
    #[cfg(feature = "codec-turbojpeg")]
    decode_scale: u32,
    /// Capture-mode size and delivered size: ROI coordinates are scaled from one to the other.
    roi_scale: ((u32, u32), (u32, u32)),
    #[cfg(feature = "codec-turbojpeg")]
    decode_threads: usize,
    roi: RoiHandle,
    pyramid_pool: BufferPool,
    /// For exportable plans: memfd buffers for the frames this stage makes, created on first use.
    #[cfg(target_os = "linux")]
    shared_pool: Option<std::sync::OnceLock<Option<SharedBufferPool>>>,
    /// Bytes of one output frame, to size `shared_pool`'s buffers.
    #[cfg(target_os = "linux")]
    output_bytes: usize,
    /// Rebuilt when the ROI changes; `None` when the route does not use turbojpeg-luma.
    #[cfg(feature = "codec-turbojpeg")]
    jpeg: Mutex<Option<(Option<FrameRect>, TurbojpegLumaDecoder)>>,
}

impl FramePreparer {
    pub(crate) fn new(plan: &FramePlan, roi: RoiHandle) -> Self {
        let luma = matches!(plan.request.format, OutputFormat::Luma);
        let output = match &plan.route {
            Route::Decode { decoder, .. } => decoder.descriptor().output,
            Route::Encode { encoder, .. } => encoder.descriptor().output,
            _ if luma => FourCc::GREY,
            _ => plan.mode.format.code,
        };
        #[cfg(feature = "codec-turbojpeg")]
        let uses_turbojpeg = matches!(&plan.route, Route::Decode { decoder, .. }
            if decoder.descriptor().impl_name == "turbojpeg-luma");
        #[allow(unused_mut)]
        let mut route = plan.route.clone();
        // An encoder (and its decoder) of its own: the registry's instances hold one stream's
        // state.
        if let Route::Encode {
            decoder,
            encoder,
            hardware,
        } = &plan.route
        {
            route = Route::Encode {
                decoder: decoder
                    .as_ref()
                    .map(|d| d.new_instance().unwrap_or_else(|| d.clone())),
                encoder: encoder.new_instance().unwrap_or_else(|| encoder.clone()),
                hardware: *hardware,
            };
        }
        // The registry's RGB decoder decodes in full; a scaled plan gets its own.
        #[cfg(feature = "codec-turbojpeg")]
        if let Route::Decode { decoder, .. } = &plan.route
            && decoder.descriptor().impl_name == "turbojpeg"
            && plan.decode_scale > 1
        {
            route = Route::Decode {
                decoder: Arc::new(
                    TurbojpegDecoder::new(output).with_scale(scale_of(plan.decode_scale)),
                ),
                hardware: false,
            };
        }
        let preparer = Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: plan.mode.format.code,
                output,
                name: "frame-plan",
                impl_name: "styx-planner",
            },
            route,
            luma,
            isp_crop: plan.region.frames_cropped(),
            isp_regions: plan.region.isp().then(|| Box::new(plan.clone())),
            view_overview: plan
                .region
                .overview
                .filter(|(_, isp)| !isp)
                .map(|(size, _)| size),
            view_regions: (1..plan.region.crops.len())
                .filter(|&i| plan.region.crops[i] == Some(super::RoiCrop::View))
                .map(|i| i as u8)
                .collect(),
            pyramid_levels: plan.request.pyramid.map_or(0, |p| p.levels),
            alignment: plan.request.row_alignment,
            #[cfg(feature = "codec-turbojpeg")]
            decode_scale: u32::from(plan.decode_scale.max(1)),
            roi_scale: (
                (
                    plan.mode.format.resolution.width.get(),
                    plan.mode.format.resolution.height.get(),
                ),
                plan.output_resolution(),
            ),
            #[cfg(feature = "codec-turbojpeg")]
            decode_threads: plan.decode_threads,
            roi,
            pyramid_pool: BufferPool::lazy(0, 8),
            #[cfg(target_os = "linux")]
            shared_pool: plan.exportable.then(std::sync::OnceLock::new),
            #[cfg(target_os = "linux")]
            output_bytes: output_bytes(plan, output),
            #[cfg(feature = "codec-turbojpeg")]
            jpeg: Mutex::new(None),
        };
        #[cfg(feature = "codec-turbojpeg")]
        if uses_turbojpeg {
            *preparer.jpeg.lock() = Some((None, preparer.luma_decoder(None)));
        }
        preparer
    }

    /// The memfd pool of an exportable plan (`None` otherwise, or if memfds are unavailable).
    #[cfg(target_os = "linux")]
    fn shared_pool(&self) -> Option<&SharedBufferPool> {
        self.shared_pool
            .as_ref()?
            .get_or_init(|| {
                // Buffers of exactly this size are recycled; a few cover the queues.
                SharedBufferPool::with_limits(0, self.output_bytes, 8)
                    .inspect_err(|err| crate::trace::warn!(error = %err, "no memfd frame pool; frames stay on the heap"))
                    .ok()
            })
            .as_ref()
    }

    /// Whether frames leave this stage as the camera delivered them (no decode, view or copy).
    #[cfg(target_os = "linux")]
    fn passes_through(&self) -> bool {
        matches!(self.route, Route::Direct) && !self.luma
    }

    /// Make the next encoded frame a keyframe (plans that encode).
    pub(crate) fn request_keyframe(&self) {
        if let Route::Encode { encoder, .. } = &self.route {
            encoder.request_keyframe();
        }
    }

    /// Decode with `decoder`, into the memfd pool when the plan is exportable.
    fn decode(&self, decoder: &dyn Codec, input: FrameLease) -> Result<FrameLease, CodecError> {
        let (clock, capture_instant) = (input.meta().clock, input.meta().capture_instant);
        let hops = input.meta().hops;
        let source = input.external_backing_handle();
        #[cfg(target_os = "linux")]
        let shared = match self.shared_pool() {
            Some(pool) => decoder.process_shared(&input, pool)?,
            None => None,
        };
        #[cfg(not(target_os = "linux"))]
        let shared = None;
        let mut frame = match shared {
            Some(frame) => frame,
            None => decoder.process(input)?,
        };
        // A conversion (or decode) into new memory, unless the decoder handed the input on.
        let same = match (&source, frame.external_backing_handle()) {
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, &b),
            _ => false,
        };
        let bytes = frame.layout_slice().iter().map(|l| l.len).sum();
        // Decoders keep the timestamp; keep the clock it is in and when it was captured, too,
        // and the hops the frame passed.
        let meta = frame.meta_mut();
        meta.clock = meta.clock.or(clock);
        meta.capture_instant = meta.capture_instant.or(capture_instant);
        if !same {
            // Decoders that carry the capture context over have the hops already.
            if meta.hops.is_empty() {
                meta.hops = hops;
            }
            styx_core::metrics::copied_frame(meta, styx_core::metrics::CopySite::Conversion, bytes);
        }
        Ok(frame)
    }

    /// `roi` (full capture-frame pixels) in the coordinates of the decoded, possibly scaled,
    /// frame, clipped to it.
    fn scaled_roi(&self, roi: FrameRect, decoded: (u32, u32)) -> Option<FrameRect> {
        let (from, to) = self.roi_scale;
        roi.scaled(from, to).clipped_to(decoded.0, decoded.1)
    }

    /// `frame` (prepared without a region) cropped to `roi`'s regions, for one of several
    /// consumers sharing it. Only luma frames are cropped, as when preparing.
    pub(crate) fn crop(
        &self,
        frame: FrameLease,
        roi: &RoiHandle,
    ) -> Result<FrameLease, CodecError> {
        if !self.luma {
            return Ok(frame);
        }
        let res = frame.meta().format.resolution;
        let decoded = (res.width.get(), res.height.get());
        let views: Vec<(u8, FrameRect)> = self
            .view_regions
            .iter()
            .filter_map(|&i| {
                let rect = roi.region(usize::from(i))?;
                Some((i, self.aligned_roi(self.scaled_roi(rect, decoded)?)))
            })
            .collect();
        let frame = super::region_frames::attach_views(frame, &views)?;
        let Some(roi) = roi.get().filter(|_| !self.isp_crop) else {
            return Ok(frame);
        };
        let Some(rect) = self.scaled_roi(roi, decoded) else {
            return Ok(frame);
        };
        let frame = frame
            .crop_view(self.aligned_roi(rect))
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        self.realign(frame)
    }

    /// This preparer for a shared capture's group, which makes each consumer's frame from the
    /// capture's itself (`region_frames::consumer_frame`) before preparing it.
    pub(crate) fn for_shared(mut self) -> Self {
        self.isp_regions = None;
        self
    }

    /// Region aligned outward so cropped rows keep the requested base alignment.
    fn aligned_roi(&self, roi: FrameRect) -> FrameRect {
        match self.alignment {
            Some(align) if align > 1 => {
                let align = align as u32;
                let x = roi.x - roi.x % align;
                FrameRect::new(x, roi.y, roi.width + (roi.x - x), roi.height)
            }
            _ => roi,
        }
    }

    /// The luma decoder for the plan, cropping to `roi` (decoded-frame pixels).
    #[cfg(feature = "codec-turbojpeg")]
    fn luma_decoder(&self, roi: Option<FrameRect>) -> TurbojpegLumaDecoder {
        TurbojpegLumaDecoder::with_options(LumaDecodeOptions {
            stride_alignment: self.alignment.unwrap_or(64),
            crop: roi.map(|r| LumaCrop {
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
            }),
            threads: self.decode_threads,
            scale: scale_of(self.decode_scale as u8),
            ..Default::default()
        })
    }

    #[cfg(feature = "codec-turbojpeg")]
    fn decode_jpeg(
        &self,
        frame: FrameLease,
        roi: Option<FrameRect>,
    ) -> Result<FrameLease, CodecError> {
        let mut guard = self.jpeg.lock();
        let entry = guard
            .as_mut()
            .ok_or_else(|| CodecError::Codec("no JPEG decoder configured".into()))?;
        if entry.0 != roi {
            *entry = (roi, self.luma_decoder(roi));
        }
        let decoder = &entry.1;
        let crop = match roi {
            Some(_) => decoder.effective_crop(frame.planes()[0].data())?,
            None => None,
        };
        let mut decoded = self.decode(decoder, frame)?;
        drop(guard);
        if let Some(c) = crop {
            decoded.meta_mut().crop = Some(FrameRect::new(c.x, c.y, c.width, c.height));
        }
        self.attach_pyramid(decoded)
    }

    /// Fill pyramid levels the ISP did not produce with box filters (keeping ISP levels).
    fn attach_pyramid(&self, frame: FrameLease) -> Result<FrameLease, CodecError> {
        if self.pyramid_levels == 0 {
            return Ok(frame);
        }
        frame
            .with_box_pyramid_in(
                self.pyramid_levels,
                self.alignment.unwrap_or(64),
                &self.pyramid_pool,
            )
            .map_err(|e| CodecError::Codec(e.to_string()))
    }

    fn realign(&self, frame: FrameLease) -> Result<FrameLease, CodecError> {
        let Some(align) = self.alignment.filter(|a| *a > 1) else {
            return Ok(frame);
        };
        if !frame.has_luma_plane() || frame.meta().format.code != FourCc::GREY {
            return Ok(frame);
        }
        let rows = frame
            .luma_rows()
            .map_err(|e| CodecError::Codec(e.to_string()))?;
        let base = rows.row(0).map_or(0, |r| r.data().as_ptr() as usize);
        if rows.stride() % align == 0 && base % align == 0 {
            return Ok(frame);
        }
        let (width, height) = (rows.row_bytes(), rows.len());
        let stride = width.next_multiple_of(align);
        #[cfg(target_os = "linux")]
        if let Some(pool) = self.shared_pool() {
            // memfd mappings are page-aligned, so rows at `stride` keep the alignment.
            let mut lease = pool
                .lease()
                .and_then(|mut lease| lease.try_resize(stride * height).map(|()| lease))
                .map_err(|e| CodecError::Codec(e.to_string()))?;
            for (y, row) in rows.iter().enumerate() {
                lease.as_mut_slice()[y * stride..y * stride + width].copy_from_slice(row.data());
            }
            let mut meta = frame.meta().clone();
            meta.residency = None;
            styx_core::metrics::copied_frame(
                &mut meta,
                styx_core::metrics::CopySite::Region,
                width * height,
            );
            let out = FrameLease::single_plane_shared(meta, lease, stride * height, stride)
                .map_err(|e| CodecError::Codec(e.to_string()))?;
            return reattach_companions(out, frame);
        }
        let mut buf = self.pyramid_pool.lease();
        buf.resize(stride * height + align - 1);
        let start = buf.as_slice().as_ptr() as usize;
        let offset = start.next_multiple_of(align) - start;
        for (y, row) in rows.iter().enumerate() {
            let dst = offset + y * stride;
            buf.as_mut_slice()[dst..dst + width].copy_from_slice(row.data());
        }
        let mut meta = frame.meta().clone();
        meta.residency = None;
        styx_core::metrics::copied_frame(
            &mut meta,
            styx_core::metrics::CopySite::Region,
            width * height,
        );
        let out = FrameLease::multi_plane(
            meta,
            smallvec![buf],
            smallvec![PlaneLayout {
                offset,
                len: stride * height,
                stride,
            }],
        );
        reattach_companions(out, frame)
    }
}

/// `out` with the companions of `source`, which it replaces.
fn reattach_companions(
    mut out: FrameLease,
    mut source: FrameLease,
) -> Result<FrameLease, CodecError> {
    for (kind, companion) in source.take_companions() {
        out = out
            .with_companion(kind, companion)
            .map_err(|e| CodecError::Codec(e.to_string()))?;
    }
    Ok(out)
}

/// Bytes of one frame the plan delivers (rows padded for alignment), at least a page.
#[cfg(target_os = "linux")]
fn output_bytes(plan: &FramePlan, output: FourCc) -> usize {
    let (w, h) = plan.output_resolution();
    let (w, h) = (w as usize, h as usize);
    if output.is_compressed() {
        // Room for a keyframe at high quality; packets are much smaller.
        return (w * h / 2).next_multiple_of(4096);
    }
    let row = match output {
        FourCc::GREY | FourCc::R8 => w,
        FourCc::YUYV => w * 2,
        FourCc::RG24 | FourCc::BG24 => w * 3,
        FourCc::NV12 | FourCc::YU12 => w.div_ceil(2) * 3,
        _ => w * 4,
    };
    let align = plan.request.row_alignment.unwrap_or(64).max(1);
    let rows = if matches!(output, FourCc::NV12 | FourCc::YU12) {
        h.div_ceil(2) * 2
    } else {
        h
    };
    (row.next_multiple_of(align) * rows).next_multiple_of(4096)
}

impl Codec for FramePreparer {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    /// ISP pyramid levels are kept (as luma views on luma routes), not computed again.
    fn handles_companions(&self) -> bool {
        true
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let roi = self.roi.get();
        let input = match &self.isp_regions {
            Some(plan) => super::region_frames::consumer_frame(input, plan)?,
            None => input,
        };
        #[cfg(feature = "codec-turbojpeg")]
        if self.jpeg.lock().is_some() {
            let full = input.meta().format.resolution;
            let decoded = (
                full.width.get().div_ceil(self.decode_scale),
                full.height.get().div_ceil(self.decode_scale),
            );
            if self.view_overview.is_some() || !self.view_regions.is_empty() {
                // The overview and the other regions are of the whole decoded frame: decode
                // it all, then crop views.
                let frame = self.decode_jpeg(input, None)?;
                return self.finish_view(frame, roi);
            }
            let roi = roi
                .and_then(|r| self.scaled_roi(r, decoded))
                .map(|r| self.aligned_roi(r));
            // The JPEG decoder crops (skipping rows below the region) and aligns itself.
            return self.decode_jpeg(input, roi);
        }
        let frame = match &self.route {
            Route::Decode { decoder, .. } => self.decode(decoder.as_ref(), input)?,
            Route::Encode {
                decoder, encoder, ..
            } => {
                let raw = match decoder {
                    Some(decoder) => {
                        let (clock, captured) = (input.meta().clock, input.meta().capture_instant);
                        let mut raw = decoder.process(input)?;
                        let meta = raw.meta_mut();
                        meta.clock = meta.clock.or(clock);
                        meta.capture_instant = meta.capture_instant.or(captured);
                        raw
                    }
                    None => input,
                };
                return self.decode(encoder.as_ref(), raw);
            }
            Route::LumaView if self.luma => luma_view(input)?,
            _ => input,
        };
        if !self.luma {
            return self.with_view_overview(frame);
        }
        self.finish_view(frame, roi)
    }
}

impl FramePreparer {
    /// A luma frame prepared without its region: with the uncropped frame as its overview when
    /// the plan attaches it here, cropped to `roi` (unless the ISP cropped it already),
    /// realigned, with its pyramid.
    fn finish_view(
        &self,
        frame: FrameLease,
        roi: Option<FrameRect>,
    ) -> Result<FrameLease, CodecError> {
        let frame = self.with_view_overview(frame)?;
        let decoded = frame.meta().format.resolution;
        let decoded = (decoded.width.get(), decoded.height.get());
        let frame = self.with_view_regions(frame, decoded)?;
        let frame = match roi
            .filter(|_| !self.isp_crop)
            .and_then(|r| self.scaled_roi(r, decoded))
        {
            Some(rect) => frame
                .crop_view(self.aligned_roi(rect))
                .map_err(|e| CodecError::Codec(e.to_string()))?,
            None => frame,
        };
        let frame = self.realign(frame)?;
        self.attach_pyramid(frame)
    }

    /// `frame` (whole, `decoded` in size) with views of the regions after the first that the
    /// plan crops here.
    fn with_view_regions(
        &self,
        frame: FrameLease,
        decoded: (u32, u32),
    ) -> Result<FrameLease, CodecError> {
        let views: Vec<(u8, FrameRect)> = self
            .view_regions
            .iter()
            .filter_map(|&i| {
                let rect = self.roi.region(usize::from(i))?;
                Some((i, self.aligned_roi(self.scaled_roi(rect, decoded)?)))
            })
            .collect();
        super::region_frames::attach_views(frame, &views)
    }

    /// `frame` with the overview the plan makes here: its luma box-filtered down to the
    /// overview's size (or the uncropped frame itself, shared without a copy, when it is that
    /// size or has no luma plane).
    fn with_view_overview(&self, frame: FrameLease) -> Result<FrameLease, CodecError> {
        let Some(size) = self.view_overview else {
            return Ok(frame);
        };
        let err = |e: FrameValidationError| CodecError::Codec(e.to_string());
        let frame = frame.into_shareable();
        let res = frame.meta().format.resolution;
        let overview = if (res.width.get(), res.height.get()) != size {
            super::region_frames::box_overview(&frame, size, &self.pyramid_pool)?
        } else {
            None
        };
        let overview = match overview {
            Some(o) => o,
            None => {
                let Some(mut whole) = frame.share() else {
                    return Ok(frame);
                };
                whole.take_companions();
                whole
            }
        };
        frame
            .with_companion(CompanionKind::Overview, overview)
            .map_err(err)
    }
}

/// The Y plane of `frame` as a GREY view, its ISP pyramid levels as well (the ISP makes them
/// in the frame's format).
fn luma_view(mut frame: FrameLease) -> Result<FrameLease, CodecError> {
    let err = |e: &dyn std::fmt::Display| CodecError::Codec(e.to_string());
    let companions = frame.take_companions();
    let mut frame = frame.into_luma().map_err(|e| err(&e))?;
    for (kind, companion) in companions {
        let companion = match kind {
            CompanionKind::Pyramid { .. }
            | CompanionKind::Overview
            | CompanionKind::Region { .. }
                if companion.has_luma_plane() =>
            {
                companion.into_luma().map_err(|e| err(&e))?
            }
            _ => companion,
        };
        frame = frame.with_companion(kind, companion).map_err(|e| err(&e))?;
    }
    Ok(frame)
}

#[cfg(feature = "codec-turbojpeg")]
fn scale_of(denom: u8) -> LumaDecodeScale {
    match denom {
        0 | 1 => LumaDecodeScale::Full,
        2 => LumaDecodeScale::Half,
        4 => LumaDecodeScale::Quarter,
        _ => LumaDecodeScale::Eighth,
    }
}
