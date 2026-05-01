use std::thread;
use std::time::{Duration, Instant};

use styx_core::prelude::{FrameLease, RecvOutcome};

use super::{MediaPipeline, MediaPipelineFrameIter};
use crate::metrics::PipelineStageError;

impl MediaPipeline {
    pub fn try_next(&mut self) -> RecvOutcome<FrameLease> {
        match self.try_next_result() {
            Ok(outcome) => outcome,
            Err(_) => RecvOutcome::Closed,
        }
    }

    /// Attempt to pull and process one frame, preserving pipeline stage errors.
    ///
    /// The infallible `try_next` facade maps stage failures to `RecvOutcome::Closed` for
    /// iterator-style callers. Use this method when production code needs the exact
    /// decode, encode, or graph failure cause.
    pub fn try_next_result(&mut self) -> Result<RecvOutcome<FrameLease>, PipelineStageError> {
        let span = tracing::trace_span!(
            "capture_stage",
            receive = "try",
            processing = "sync",
            worker = "caller"
        );
        let _enter = span.enter();
        let capture_start = Instant::now();
        match self.capture.recv() {
            RecvOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                self.process_frame_result(frame).map(RecvOutcome::Data)
            }
            RecvOutcome::Empty => Ok(RecvOutcome::Empty),
            RecvOutcome::Closed => Ok(RecvOutcome::Closed),
        }
    }

    pub fn next_blocking(&mut self, wait: Duration) -> RecvOutcome<FrameLease> {
        match self.next_blocking_result(wait) {
            Ok(outcome) => outcome,
            Err(_) => RecvOutcome::Closed,
        }
    }

    /// Wait for a frame up to `wait`, preserving pipeline stage errors.
    pub fn next_blocking_result(
        &mut self,
        wait: Duration,
    ) -> Result<RecvOutcome<FrameLease>, PipelineStageError> {
        let span = tracing::trace_span!(
            "capture_stage",
            receive = "blocking_timeout",
            processing = "sync",
            worker = "caller",
            timeout_ms = wait.as_millis() as u64
        );
        let _enter = span.enter();
        let capture_start = Instant::now();
        match self.capture.recv_timeout(wait) {
            styx_core::queue::RecvWaitOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                self.process_frame_result(frame).map(RecvOutcome::Data)
            }
            styx_core::queue::RecvWaitOutcome::Closed => Ok(RecvOutcome::Closed),
            styx_core::queue::RecvWaitOutcome::Timeout => Ok(RecvOutcome::Empty),
        }
    }

    /// Wait indefinitely for the next processed frame.
    pub fn next_forever(&mut self) -> RecvOutcome<FrameLease> {
        match self.next_forever_result() {
            Ok(outcome) => outcome,
            Err(_) => RecvOutcome::Closed,
        }
    }

    /// Wait indefinitely for the next processed frame, preserving pipeline stage errors.
    pub fn next_forever_result(&mut self) -> Result<RecvOutcome<FrameLease>, PipelineStageError> {
        let span = tracing::trace_span!(
            "capture_stage",
            receive = "blocking_forever",
            processing = "sync",
            worker = "caller"
        );
        let _enter = span.enter();
        let capture_start = Instant::now();
        match self.capture.recv_forever() {
            RecvOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                self.process_frame_result(frame).map(RecvOutcome::Data)
            }
            RecvOutcome::Empty => Ok(RecvOutcome::Empty),
            RecvOutcome::Closed => Ok(RecvOutcome::Closed),
        }
    }

    /// Iterate processed frames, ignoring timeout wakeups and ending when capture closes.
    pub fn frames_blocking(&mut self, wait: Duration) -> MediaPipelineFrameIter<'_> {
        MediaPipelineFrameIter::new(self, wait)
    }

    #[cfg(feature = "async")]
    /// Await the next captured frame, then run pipeline processing on the current task.
    ///
    /// Decode, encode, graph, and hook stages are still synchronous CPU work. Use
    /// `spawn_blocking_worker` for CPU-heavy decode/encode pipelines in Tokio applications.
    pub async fn next_async(&mut self) -> RecvOutcome<FrameLease> {
        match self.next_async_result().await {
            Ok(outcome) => outcome,
            Err(_) => RecvOutcome::Closed,
        }
    }

    #[cfg(feature = "async")]
    /// Await the next captured frame and preserve pipeline stage errors.
    pub async fn next_async_result(
        &mut self,
    ) -> Result<RecvOutcome<FrameLease>, PipelineStageError> {
        let span = tracing::trace_span!(
            "capture_stage",
            receive = "async",
            processing = "sync",
            worker = "tokio_task"
        );
        let _enter = span.enter();
        let capture_start = Instant::now();
        match self.capture.recv_async().await {
            RecvOutcome::Data(frame) => {
                self.metrics.capture.record(capture_start.elapsed());
                self.process_frame_result(frame).map(RecvOutcome::Data)
            }
            RecvOutcome::Empty => Ok(RecvOutcome::Empty),
            RecvOutcome::Closed => Ok(RecvOutcome::Closed),
        }
    }

    #[cfg(feature = "async")]
    /// Spawn a lightweight async receive loop.
    ///
    /// This keeps frame receipt async, but each received frame is processed synchronously by
    /// `next_async` on a Tokio core worker. Prefer `spawn_tokio_worker` or
    /// `spawn_blocking_worker` for normal decode, encode, graph, hook, or sink pipelines.
    pub fn spawn_async_worker(mut self) -> tokio::task::JoinHandle<()> {
        tokio::task::spawn(async move {
            let span = tracing::trace_span!(
                "pipeline_worker",
                worker = "tokio_task",
                receive = "async",
                processing = "sync"
            );
            let _enter = span.enter();
            loop {
                match self.next_async().await {
                    RecvOutcome::Data(_) => {}
                    RecvOutcome::Empty => {}
                    RecvOutcome::Closed => {
                        self.capture.stop_in_place();
                        break;
                    }
                }
            }
        })
    }

    #[cfg(feature = "async")]
    /// Spawn the pipeline on Tokio's blocking pool.
    ///
    /// This is the recommended Tokio worker for CPU-heavy decode, encode, graph, or hook
    /// stages because processing runs outside core async runtime workers.
    pub fn spawn_blocking_worker(self) -> tokio::task::JoinHandle<()> {
        tokio::task::spawn_blocking(move || self.run_blocking_worker())
    }

    #[cfg(feature = "async")]
    /// Spawn the recommended Tokio pipeline worker.
    ///
    /// This is an ergonomic alias for `spawn_blocking_worker`; it keeps synchronous media
    /// processing off Tokio core runtime workers while still returning a Tokio join handle.
    pub fn spawn_tokio_worker(self) -> tokio::task::JoinHandle<()> {
        self.spawn_blocking_worker()
    }

    pub fn spawn_worker(self) -> thread::JoinHandle<()> {
        thread::spawn(move || self.run_blocking_worker())
    }

    fn run_blocking_worker(mut self) {
        let span = tracing::trace_span!(
            "pipeline_worker",
            worker = "thread",
            receive = "blocking_forever",
            processing = "sync"
        );
        let _enter = span.enter();
        loop {
            match self.next_forever() {
                RecvOutcome::Data(_) => {}
                RecvOutcome::Empty => {}
                RecvOutcome::Closed => {
                    self.capture.stop_in_place();
                    break;
                }
            }
        }
    }
}
