//! Running a [`FramePlan`]: capture, then a single preparation stage that applies the planned
//! route (decode or luma view), region of interest, pyramid levels and row alignment.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::smallvec;
#[cfg(feature = "codec-turbojpeg")]
use styx_codec::prelude::{
    LumaCrop, LumaDecodeOptions, LumaDecodeScale, TurbojpegDecoder, TurbojpegLumaDecoder,
};
use styx_codec::{Codec, CodecDescriptor, CodecError, CodecKind};
use styx_core::prelude::*;

use super::shared::SharedCapture;
use super::{FramePlan, Route};
use crate::capture_api::{CaptureError, CaptureRequest, StyxConfig};
use crate::session::{MediaPipeline, MediaPipelineBuilder};
use styx_core::queue::BoundedRx;

/// Live region-of-interest control for a running plan. Cloneable; changes apply to the next
/// frame. Coordinates are full-frame pixels.
#[derive(Clone, Default)]
pub struct RoiHandle(Arc<Mutex<Option<FrameRect>>>);

impl RoiHandle {
    pub fn set(&self, roi: Option<FrameRect>) {
        *self.0.lock() = roi;
    }

    pub fn get(&self) -> Option<FrameRect> {
        *self.0.lock()
    }
}

/// Frames delivered according to a [`FramePlan`].
pub struct PlannedFrames {
    source: Source,
    roi: RoiHandle,
    plan: FramePlan,
}

enum Source {
    /// The plan's own capture.
    Pipeline(Box<MediaPipeline>),
    /// One consumer of a capture shared through a [`super::SharedFramePlan`].
    Branch(Box<Branch>),
}

struct Branch {
    shared: Arc<SharedCapture>,
    index: usize,
    rx: BoundedRx<FrameLease>,
    preparer: FramePreparer,
}

impl Drop for Branch {
    fn drop(&mut self) {
        // Frames shared with a consumer that is gone would hold camera buffers.
        self.rx.close();
    }
}

impl PlannedFrames {
    pub(crate) fn branch(
        plan: &FramePlan,
        shared: Arc<SharedCapture>,
        index: usize,
        rx: BoundedRx<FrameLease>,
    ) -> Self {
        let roi = RoiHandle::default();
        roi.set(plan.requirements.roi);
        let preparer = FramePreparer::new(plan, roi.clone());
        Self {
            source: Source::Branch(Box::new(Branch {
                shared,
                index,
                rx,
                preparer,
            })),
            roi,
            plan: plan.clone(),
        }
    }

    /// Next frame, waiting up to `wait`. A frame that fails to prepare (e.g. a corrupt JPEG) is
    /// skipped on a shared capture and ends a plan's own pipeline.
    pub fn next_frame(&mut self, wait: Duration) -> RecvOutcome<FrameLease> {
        match &mut self.source {
            Source::Pipeline(pipeline) => pipeline.next_blocking(wait),
            Source::Branch(branch) => match branch.shared.next(branch.index, &branch.rx, wait) {
                RecvOutcome::Data(frame) => match branch.preparer.process(frame) {
                    Ok(frame) => RecvOutcome::Data(frame),
                    Err(err) => {
                        tracing::warn!(consumer = branch.index, error = %err, "frame skipped");
                        RecvOutcome::Empty
                    }
                },
                other => other,
            },
        }
    }

    /// Change the region of interest while running (`None` = full frame).
    pub fn roi(&self) -> RoiHandle {
        self.roi.clone()
    }

    pub fn plan(&self) -> &FramePlan {
        &self.plan
    }

    /// The underlying pipeline of a plan's own capture (`None` on a shared capture).
    pub fn pipeline(&mut self) -> Option<&mut MediaPipeline> {
        match &mut self.source {
            Source::Pipeline(pipeline) => Some(pipeline),
            Source::Branch(_) => None,
        }
    }

    /// Health of the capture; on a shared capture, with this consumer's own dropped frames
    /// (those it was too slow to take) counted as queue evictions.
    pub fn health_report(&self) -> crate::metrics::HealthReport {
        match &self.source {
            Source::Pipeline(pipeline) => pipeline.health_report(),
            Source::Branch(branch) => {
                let mut report = branch.shared.capture().health_report();
                let evictions = branch.rx.stats().evictions;
                crate::metrics::push_drop_reason(
                    &mut report.drop_reasons,
                    crate::metrics::FrameDropReason::CaptureQueueEviction,
                    evictions,
                );
                report.drop_count = crate::metrics::total_frame_drops(&report.drop_reasons);
                report
            }
        }
    }

    pub fn stop(self) {
        if let Source::Pipeline(pipeline) = self.source {
            pipeline.stop();
        }
    }
}

impl Iterator for PlannedFrames {
    type Item = FrameLease;

    fn next(&mut self) -> Option<FrameLease> {
        loop {
            match self.next_frame(Duration::from_secs(1)) {
                RecvOutcome::Data(frame) => return Some(frame),
                RecvOutcome::Empty => {}
                RecvOutcome::Closed => return None,
            }
        }
    }
}

impl FramePlan {
    /// Start capturing with this plan.
    pub fn start(&self) -> Result<PlannedFrames, CaptureError> {
        let mut config = StyxConfig::new().capture_queue_depth(self.queue_depth);
        if let Some(level) = self.isp_pyramid_level {
            config = config.libcamera_pyramid_level(level);
        }
        if let Some((width, height)) = self.isp_output {
            config = config.libcamera_output_size(width, height);
        }
        if let Some(after) = self.stop_when_idle {
            config = config.stop_when_idle(after);
        }
        let mut capture = CaptureRequest::new(&self.device)
            .backend(self.backend)
            .mode(self.mode.id.clone())
            .config(config);
        if let Some(interval) = self.interval {
            capture = capture.interval(interval);
        }
        let roi = RoiHandle::default();
        roi.set(self.requirements.roi);
        let preparer = FramePreparer::new(self, roi.clone());
        let builder = MediaPipelineBuilder::new(capture).decoder(Arc::new(preparer));
        // Planned frames are for an in-process consumer: zero-copy views and pooled buffers,
        // not memfd/dma-buf exports for other processes.
        #[cfg(target_os = "linux")]
        let builder = builder.shared_decode_output(false);
        let pipeline = builder.start()?;
        Ok(PlannedFrames {
            source: Source::Pipeline(Box::new(pipeline)),
            roi,
            plan: self.clone(),
        })
    }
}

/// One pipeline stage carrying out everything after capture.
struct FramePreparer {
    descriptor: CodecDescriptor,
    route: Route,
    luma: bool,
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
    /// Rebuilt when the ROI changes; `None` when the route does not use turbojpeg-luma.
    #[cfg(feature = "codec-turbojpeg")]
    jpeg: Mutex<Option<(Option<FrameRect>, TurbojpegLumaDecoder)>>,
}

impl FramePreparer {
    fn new(plan: &FramePlan, roi: RoiHandle) -> Self {
        let luma = matches!(plan.requirements.output, OutputFormat::Luma);
        let output = match &plan.route {
            Route::Decode { decoder, .. } => decoder.descriptor().output,
            _ if luma => FourCc::GREY,
            _ => plan.mode.format.code,
        };
        #[cfg(feature = "codec-turbojpeg")]
        let uses_turbojpeg = matches!(&plan.route, Route::Decode { decoder, .. }
            if decoder.descriptor().impl_name == "turbojpeg-luma");
        #[allow(unused_mut)]
        let mut route = plan.route.clone();
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
            pyramid_levels: plan.requirements.pyramid.map_or(0, |p| p.levels),
            alignment: plan.requirements.stride_alignment,
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
            #[cfg(feature = "codec-turbojpeg")]
            jpeg: Mutex::new(None),
        };
        #[cfg(feature = "codec-turbojpeg")]
        if uses_turbojpeg {
            *preparer.jpeg.lock() = Some((None, preparer.luma_decoder(None)));
        }
        preparer
    }

    /// `roi` (full capture-frame pixels) in the coordinates of the decoded, possibly scaled,
    /// frame, clipped to it.
    fn scaled_roi(&self, roi: FrameRect, decoded: (u32, u32)) -> Option<FrameRect> {
        let ((from_w, from_h), (to_w, to_h)) = self.roi_scale;
        let (fw, fh, tw, th) = (
            u64::from(from_w),
            u64::from(from_h),
            u64::from(to_w),
            u64::from(to_h),
        );
        // Outward: the scaled region covers every pixel of the requested one.
        let x = (u64::from(roi.x) * tw / fw) as u32;
        let y = (u64::from(roi.y) * th / fh) as u32;
        let right = (u64::from(roi.x + roi.width) * tw).div_ceil(fw) as u32;
        let bottom = (u64::from(roi.y + roi.height) * th).div_ceil(fh) as u32;
        FrameRect::new(x, y, right - x, bottom - y).clipped_to(decoded.0, decoded.1)
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
        let mut decoded = decoder.process(frame)?;
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
        let mut out = FrameLease::multi_plane(
            meta,
            smallvec![buf],
            smallvec![PlaneLayout {
                offset,
                len: stride * height,
                stride,
            }],
        );
        let mut source = frame;
        for (kind, companion) in source.take_companions() {
            out = out
                .with_companion(kind, companion)
                .map_err(|e| CodecError::Codec(e.to_string()))?;
        }
        Ok(out)
    }
}

impl Codec for FramePreparer {
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
        let roi = self.roi.get();
        #[cfg(feature = "codec-turbojpeg")]
        if self.jpeg.lock().is_some() {
            let full = input.meta().format.resolution;
            let decoded = (
                full.width.get().div_ceil(self.decode_scale),
                full.height.get().div_ceil(self.decode_scale),
            );
            let roi = roi
                .and_then(|r| self.scaled_roi(r, decoded))
                .map(|r| self.aligned_roi(r));
            // The JPEG decoder crops (skipping rows below the region) and aligns itself.
            return self.decode_jpeg(input, roi);
        }
        let frame = match &self.route {
            Route::Decode { decoder, .. } => decoder.process(input)?,
            Route::LumaView if self.luma => input
                .into_luma()
                .map_err(|e| CodecError::Codec(e.to_string()))?,
            _ => input,
        };
        if !self.luma {
            return Ok(frame);
        }
        let decoded = frame.meta().format.resolution;
        let frame = match roi
            .and_then(|r| self.scaled_roi(r, (decoded.width.get(), decoded.height.get())))
        {
            Some(rect) => frame
                .crop_view(self.aligned_roi(rect))
                .map_err(|e| CodecError::Codec(e.to_string()))?,
            None => frame,
        };
        let frame = self.realign(frame)?;
        self.attach_pyramid(frame)
    }
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
