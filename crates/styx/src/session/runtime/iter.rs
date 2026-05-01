use std::time::Duration;

use styx_core::prelude::*;

use super::MediaPipeline;

/// Blocking frame iterator returned by `MediaPipeline::frames_blocking`.
pub struct MediaPipelineFrameIter<'a> {
    pipeline: &'a mut MediaPipeline,
    wait: Duration,
    remaining: Option<usize>,
    closed: bool,
}

impl<'a> MediaPipelineFrameIter<'a> {
    pub(super) fn new(pipeline: &'a mut MediaPipeline, wait: Duration) -> Self {
        Self {
            pipeline,
            wait,
            remaining: None,
            closed: false,
        }
    }

    /// Limit the iterator to at most `count` frames.
    pub fn take_frames(mut self, count: usize) -> Self {
        self.remaining = Some(count);
        self
    }
}

impl Iterator for MediaPipelineFrameIter<'_> {
    type Item = FrameLease;

    fn next(&mut self) -> Option<Self::Item> {
        if self.closed || self.remaining == Some(0) {
            return None;
        }
        loop {
            let outcome = if self.wait.is_zero() {
                self.pipeline.next_forever()
            } else {
                self.pipeline.next_blocking(self.wait)
            };
            match outcome {
                RecvOutcome::Data(frame) => {
                    if let Some(remaining) = &mut self.remaining {
                        *remaining = remaining.saturating_sub(1);
                    }
                    return Some(frame);
                }
                RecvOutcome::Empty => continue,
                RecvOutcome::Closed => {
                    self.closed = true;
                    return None;
                }
            }
        }
    }
}
