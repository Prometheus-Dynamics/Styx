//! [`CaptureHandle::poll_recv`]: a capture's frames from a hand-written future or `Stream`.

use std::task::{Context, Poll};
use std::time::Instant;

use styx_core::prelude::{FrameLease, RecvOutcome};

use super::handle::CaptureHandle;

impl CaptureHandle {
    /// A frame (`Data`), `Closed` once the capture closed and drained, or `Pending` with `cx`'s
    /// waker registered: it is woken by the next frame or by the capture closing. Any executor;
    /// the waker may also write an eventfd, so frames wake a `poll`/`epoll` loop
    /// (`styx::multicam::FrameGrouper` does). Counted like [`Self::recv`].
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<RecvOutcome<FrameLease>> {
        let start = Instant::now();
        let _demand = self.demand();
        match self.rx.poll_recv(cx) {
            Poll::Ready(RecvOutcome::Data(mut frame)) => {
                self.took(start, &mut frame);
                Poll::Ready(RecvOutcome::Data(frame))
            }
            other => other,
        }
    }
}
