//! Running a [`FramePlan`]: capture, then a single preparation stage that applies the planned
//! route (decode or luma view), region of interest, pyramid levels and row alignment.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::smallvec;
#[cfg(feature = "codec-turbojpeg")]
use styx_codec::prelude::{LumaCrop, LumaDecodeOptions, TurbojpegLumaDecoder};
use styx_codec::{Codec, CodecDescriptor, CodecError, CodecKind};
use styx_core::prelude::*;

use super::{FramePlan, Route};
use crate::capture_api::{CaptureError, CaptureRequest, StyxConfig};
use crate::session::{MediaPipeline, MediaPipelineBuilder};

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
    pipeline: MediaPipeline,
    roi: RoiHandle,
    plan: FramePlan,
}

impl PlannedFrames {
    /// Next frame, waiting up to `wait`.
    pub fn next_frame(&mut self, wait: Duration) -> RecvOutcome<FrameLease> {
        self.pipeline.next_blocking(wait)
    }

    /// Change the region of interest while running (`None` = full frame).
    pub fn roi(&self) -> RoiHandle {
        self.roi.clone()
    }

    pub fn plan(&self) -> &FramePlan {
        &self.plan
    }

    /// The underlying pipeline, for metrics and health reports.
    pub fn pipeline(&mut self) -> &mut MediaPipeline {
        &mut self.pipeline
    }

    pub fn stop(self) {
        self.pipeline.stop();
    }
}

impl Iterator for PlannedFrames {
    type Item = FrameLease;

    fn next(&mut self) -> Option<FrameLease> {
        self.pipeline.next()
    }
}

impl FramePlan {
    /// Start capturing with this plan.
    pub fn start(&self) -> Result<PlannedFrames, CaptureError> {
        let mut config = StyxConfig::new().capture_queue_depth(self.queue_depth);
        if let Some(level) = self.isp_pyramid_level {
            config = config.libcamera_pyramid_level(level);
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
            pipeline,
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
        Self {
            descriptor: CodecDescriptor {
                kind: CodecKind::Decoder,
                input: plan.mode.format.code,
                output,
                name: "frame-plan",
                impl_name: "styx-planner",
            },
            route: plan.route.clone(),
            luma,
            pyramid_levels: plan.requirements.pyramid.map_or(0, |p| p.levels),
            alignment: plan.requirements.stride_alignment,
            #[cfg(feature = "codec-turbojpeg")]
            decode_threads: plan.decode_threads,
            roi,
            pyramid_pool: BufferPool::lazy(0, 8),
            #[cfg(feature = "codec-turbojpeg")]
            jpeg: Mutex::new(uses_turbojpeg.then(|| (None, TurbojpegLumaDecoder::new()))),
        }
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
            let options = LumaDecodeOptions {
                stride_alignment: self.alignment.unwrap_or(64),
                crop: roi.map(|r| LumaCrop {
                    x: r.x,
                    y: r.y,
                    width: r.width,
                    height: r.height,
                }),
                threads: self.decode_threads,
                ..Default::default()
            };
            *entry = (roi, TurbojpegLumaDecoder::with_options(options));
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
            let roi = roi
                .and_then(|r| r.clipped_to(full.width.get(), full.height.get()))
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
        let frame = match roi {
            Some(rect) => frame
                .crop_view(self.aligned_roi(rect))
                .map_err(|e| CodecError::Codec(e.to_string()))?,
            None => frame,
        };
        let frame = self.realign(frame)?;
        self.attach_pyramid(frame)
    }
}
