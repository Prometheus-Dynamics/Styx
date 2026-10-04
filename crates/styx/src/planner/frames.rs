//! [`Frames`]: a running stream of planned frames, from a plan's own capture or as one consumer
//! of a shared one.

use std::sync::Arc;
use std::time::Duration;

use styx_core::prelude::*;

use super::FramePlan;
use super::roi::RoiHandle;
use super::session::Branch;
use super::start::FramePreparer;
use crate::capture_api::{CaptureError, CaptureHandle};
use crate::session::MediaPipeline;

/// A running stream of frames, as a [`FrameRequest`](super::FrameRequest) asked for: from
/// [`FrameRequest::open`](super::FrameRequest::open) (`Frames::nv12().size(..).open(&camera)`),
/// [`FramePlan::start`] or, one per consumer, [`SharedFramePlan::start`](super::SharedFramePlan::start).
/// Take frames with [`Frames::next_frame`] (or as an iterator, or
/// [`Frames::next_frame_async`]), set camera controls with [`Frames::set_control`].
pub struct Frames {
    source: Source,
    roi: RoiHandle,
    plan: FramePlan,
    /// The preparation stage of a plan's own pipeline.
    preparer: Option<Arc<FramePreparer>>,
}

enum Source {
    /// The plan's own capture.
    Pipeline(Box<MediaPipeline>),
    /// One consumer of a capture shared through a [`super::SharedFramePlan`].
    Branch(Box<Branch>),
}

/// The previous name of [`Frames`].
#[deprecated(note = "renamed to Frames")]
pub type PlannedFrames = Frames;

impl Frames {
    /// The frames of a plan's own capture, prepared by `preparer`.
    pub(crate) fn of_pipeline(
        pipeline: MediaPipeline,
        roi: RoiHandle,
        plan: FramePlan,
        preparer: Arc<FramePreparer>,
    ) -> Self {
        Self {
            source: Source::Pipeline(Box::new(pipeline)),
            roi,
            plan,
            preparer: Some(preparer),
        }
    }

    pub(crate) fn branch(plan: &FramePlan, branch: Branch, roi: RoiHandle) -> Self {
        Self {
            source: Source::Branch(Box::new(branch)),
            roi,
            plan: plan.clone(),
            preparer: None,
        }
    }

    /// Next frame, waiting up to `wait`. A frame that fails to prepare (e.g. a corrupt JPEG) is
    /// skipped on a shared capture and ends a plan's own pipeline.
    ///
    /// With [`FrameRequest::skip_stale_regions`](super::FrameRequest::skip_stale_regions),
    /// frames whose ISP crops do not cover the regions as set now are skipped.
    pub fn next_frame(&mut self, wait: Duration) -> RecvOutcome<FrameLease> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let outcome = match &mut self.source {
                Source::Pipeline(pipeline) => pipeline.next_blocking(left),
                Source::Branch(branch) => branch.next(left),
            };
            match outcome {
                RecvOutcome::Data(frame)
                    if !super::region_frames::fresh(&frame, &self.plan, &self.roi) =>
                {
                    if left.is_zero() {
                        return RecvOutcome::Empty;
                    }
                }
                other => return other,
            }
        }
    }

    /// Await the next frame, or `Closed` when the capture ends. The frame is prepared (decoded,
    /// scaled) on the calling task, as `MediaPipeline::next_async_receive` does; move heavy
    /// plans to a blocking task.
    #[cfg(feature = "async")]
    pub async fn next_frame_async(&mut self) -> RecvOutcome<FrameLease> {
        loop {
            let outcome = match &mut self.source {
                Source::Pipeline(pipeline) => pipeline.next_async_receive().await,
                Source::Branch(branch) => branch.next_async().await,
            };
            match outcome {
                RecvOutcome::Data(frame)
                    if !super::region_frames::fresh(&frame, &self.plan, &self.roi) => {}
                other => return other,
            }
        }
    }

    /// For plans that encode H.264/H.265: make the next packet a keyframe, e.g. when a viewer
    /// starts or lost packets. Consumers of a shared capture ask for one on their own when they
    /// join or fall behind.
    pub fn request_keyframe(&self) {
        match &self.source {
            Source::Pipeline(_) => {
                if let Some(preparer) = &self.preparer {
                    preparer.request_keyframe();
                }
            }
            Source::Branch(branch) => branch.request_keyframe(),
        }
    }

    /// Change the region of interest while running (`None` = full frame).
    pub fn roi(&self) -> RoiHandle {
        self.roi.clone()
    }

    pub fn plan(&self) -> &FramePlan {
        &self.plan
    }

    /// The camera capture behind these frames (shared with the other consumers of a shared
    /// capture): controls, mode and interval, metrics.
    pub fn capture(&self) -> &CaptureHandle {
        match &self.source {
            Source::Pipeline(pipeline) => pipeline.capture(),
            Source::Branch(branch) => branch.capture(),
        }
    }

    /// Set a camera control (exposure, gain, white balance, ...; see the backend's controls).
    /// On a shared capture it applies to every consumer's frames.
    pub fn set_control(&self, id: ControlId, value: ControlValue) -> Result<(), CaptureError> {
        self.capture().set_control(id, value)
    }

    /// A camera control's current value.
    pub fn get_control(&self, id: ControlId) -> Result<ControlValue, CaptureError> {
        self.capture().get_control(id)
    }

    /// Frames lost so far: dropped because this consumer did not take them in time (beyond its
    /// queue: one frame with [`Delivery::Latest`](super::Delivery::Latest), `n` with
    /// `EveryFrame(n)`), or by the capture. Nothing is lost silently: the health report says
    /// where.
    pub fn dropped(&self) -> u64 {
        self.health_report().drop_count
    }

    /// The capture's metrics (`docs/metrics.md`): rate, drops by cause, latency, ISP and CPU
    /// time, 3A, buffers; on a shared capture with every consumer listed.
    pub fn metrics(&self) -> crate::metrics::CameraMetrics {
        self.capture().camera_metrics()
    }

    /// This consumer's own counts on a shared capture: frames received, frames it did not take
    /// in time (`None` for a plan's own capture: see [`Frames::metrics`]).
    pub fn consumer_metrics(&self) -> Option<crate::metrics::ConsumerMetrics> {
        match &self.source {
            Source::Pipeline(_) => None,
            Source::Branch(branch) => Some(branch.consumer_metrics()),
        }
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
            Source::Branch(branch) => branch.health_report(),
        }
    }

    pub fn stop(self) {
        if let Source::Pipeline(pipeline) = self.source {
            pipeline.stop();
        }
    }
}

impl Iterator for Frames {
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
