//! Sliced frames handed out at their first slice (`NativeIspConfig::deliver_slices`): the
//! frame's leases go to the consumer as soon as the back end's first job has written the top of
//! its buffers, with a shared [`FrameProgress`] that says how many rows are final; the frame
//! completes (or fails) on the same leases when the rest is written.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use smallvec::SmallVec;
use styx_core::buffer::FrameProgress;
use styx_kernel::dma_heap::{self, Access};
use styx_pipeline::device::{PispFrame, PispPipeline, PispSlice};
use styx_pisp::device::BeJob;

use super::pisp_lease::{Buffers, Returns};
use super::pisp_worker::{Worker, frame_leases};
use styx_capture::prelude::FrameLease;

/// A sliced frame's state shared by its leases: its progress, and whether any of them was read
/// on the CPU before it completed (only then can the CPU hold stale lines of rows below the
/// first slice, so only then are the buffers synced again on completion).
pub(super) struct SliceState {
    pub(super) progress: Arc<FrameProgress>,
    pub(super) read: AtomicBool,
}

/// A sliced frame out with the consumer before it completes.
pub(super) struct SlicedOut {
    pub(super) sequence: u64,
    state: Arc<SliceState>,
    /// (output, buffer) of its leases.
    buffers: [Option<(usize, u32)>; 2],
    heights: [u32; 2],
    /// Buffers its leases gave back early: the back end still writes them until the frame is
    /// done, then they are released.
    deferred: SmallVec<[(usize, u32); 2]>,
}

impl SlicedOut {
    /// Whether `(output, buffer)` is one of the frame's.
    pub(super) fn holds(&self, i: usize, index: u32) -> bool {
        self.buffers.contains(&Some((i, index)))
    }

    pub(super) fn defer(&mut self, i: usize, index: u32) {
        self.deferred.push((i, index));
    }

    fn release(&mut self, p: &mut PispPipeline) {
        for (i, index) in self.deferred.drain(..) {
            p.release_output(i, index);
        }
    }

    /// The frame is written: if a lease was read on the CPU meanwhile, the buffers' caches are
    /// invalidated again (the CPU may have prefetched rows below the first slice before the back
    /// end wrote them), then the leases see every row. Frames only passed on (a camera
    /// service's) skip the sync.
    pub(super) fn complete(mut self, buffers: &Buffers, p: &mut PispPipeline) {
        if self.state.read.load(Ordering::Acquire) {
            for &(i, index) in self.buffers.iter().flatten() {
                if let Some((fd, true)) = buffers.fd(i, index) {
                    let _ = dma_heap::sync(fd, Access::Read, true);
                }
            }
        }
        self.state.progress.complete(self.heights);
        self.release(p);
    }

    /// The frame will not complete (it failed, or a newer one overtook it).
    pub(super) fn fail(mut self, p: &mut PispPipeline) {
        self.state.progress.fail();
        self.release(p);
    }
}

impl Drop for SlicedOut {
    /// A frame dropped without [`SlicedOut::complete`] or [`SlicedOut::fail`] (a worker that
    /// ended) fails, so nothing waits for it; after either, this changes nothing.
    fn drop(&mut self) {
        self.state.progress.fail();
    }
}

/// The leases of a sliced frame's first slice, and what completes them later. `None` when the
/// frame's sensor values are unknown (the frame then just arrives whole).
pub(super) fn slice_leases(
    p: &PispPipeline,
    s: &PispSlice,
    buffers: &mut Buffers,
    w: &Worker,
    returns: &Returns,
) -> Result<Option<(FrameLease, SlicedOut)>, String> {
    let Some(sensor) = p.frame_values(s.sequence) else {
        return Ok(None);
    };
    // The frame as far as it is known: its timestamp (the start of frame), sensor values and
    // buffers; the back end's times come with the frame.
    let f = PispFrame {
        sequence: s.sequence,
        timestamp: s.timestamp,
        dequeued: Instant::now(),
        sensor,
        job: BeJob {
            outputs: s.outputs,
            elapsed: Duration::ZERO,
        },
        sequence_mismatch: false,
        request_lands: None,
        settings_from: None,
        digital_gain: 1.0,
        flicker: 1.0,
        times: Default::default(),
        raw: None,
        passes: Vec::new(),
    };
    let state = Arc::new(SliceState {
        progress: Arc::new(FrameProgress::new(s.rows)),
        read: AtomicBool::new(false),
    });
    let Some(lease) = frame_leases(p, &f, buffers, w, returns, Some(state.clone()))? else {
        return Ok(None);
    };
    let heights = [0, 1].map(|i| w.specs[i].map_or(0, |o| o.height));
    let bufs = [0, 1].map(|i| s.outputs[i].map(|b| (i, b)));
    Ok(Some((
        lease,
        SlicedOut {
            sequence: s.sequence,
            state,
            buffers: bufs,
            heights,
            deferred: SmallVec::new(),
        },
    )))
}
