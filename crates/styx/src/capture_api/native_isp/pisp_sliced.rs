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

use super::pisp_lease::{Buffers, RAW_OUTPUT, Returns};
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
    /// Its raw frame leased with it (`NativeIspConfig::sensor_raw`): the progress of that
    /// lease, the front end buffer and the frame's height.
    raw: Option<(Arc<SliceState>, u32, u32)>,
}

impl SlicedOut {
    /// Whether `(output, buffer)` is one of the frame's.
    pub(super) fn holds(&self, i: usize, index: u32) -> bool {
        self.buffers.contains(&Some((i, index)))
            || (i == RAW_OUTPUT && self.raw.as_ref().is_some_and(|r| r.1 == index))
    }

    /// Whether its raw frame went out with it.
    pub(super) fn has_raw(&self) -> bool {
        self.raw.is_some()
    }

    pub(super) fn defer(&mut self, i: usize, index: u32) {
        self.deferred.push((i, index));
    }

    /// Releases the output buffers given back early; whether the raw one was among them.
    fn release(&mut self, p: &mut PispPipeline) -> bool {
        let mut raw_back = false;
        for (i, index) in self.deferred.drain(..) {
            if i == RAW_OUTPUT {
                raw_back = true;
            } else {
                p.release_output(i, index);
            }
        }
        raw_back
    }

    /// The frame is written: if a lease was read on the CPU meanwhile, the buffers' caches are
    /// invalidated again (the CPU may have prefetched rows below the first slice before the back
    /// end wrote them), then the leases see every row. Frames only passed on (a camera
    /// service's) skip the sync.
    ///
    /// `frame_raw` is the raw buffer the completed frame holds (`PispFrame::raw_image`). Returns
    /// whether the raw lease made at the first slice took it (the frame's raw buffer is then
    /// that lease's, not to be given back now), and a raw lease buffer to ignore when it
    /// returns ([`Self::orphan_raw`]).
    pub(super) fn complete(
        mut self,
        buffers: &Buffers,
        p: &mut PispPipeline,
        frame_raw: Option<u32>,
    ) -> (bool, Option<u32>) {
        if self.state.read.load(Ordering::Acquire) {
            for &(i, index) in self.buffers.iter().flatten() {
                if let Some((fd, true)) = buffers.fd(i, index) {
                    let _ = dma_heap::sync(fd, Access::Read, true);
                }
            }
        }
        self.state.progress.complete(self.heights);
        let raw_back = self.release(p);
        let Some((raw, index, height)) = self.raw.clone() else {
            return (false, None);
        };
        match raw_end(index, raw_back, Some(frame_raw)) {
            RawEnd::Frame { release_now } => {
                if raw.read.load(Ordering::Acquire)
                    && let Some((fd, _)) = buffers.fd(RAW_OUTPUT, index)
                {
                    let _ = dma_heap::sync(fd, Access::Read, true);
                }
                raw.progress.complete([height, 0]);
                if release_now {
                    let _ = p.release_raw(index);
                }
                (true, None)
            }
            RawEnd::Orphan(orphan) => {
                raw.progress.fail();
                (false, orphan)
            }
        }
    }

    /// The frame will not complete (it failed, or a newer one overtook it). Returns a raw lease
    /// buffer to ignore when it returns: the front end has that buffer back (the frame that
    /// would have held it never came).
    pub(super) fn fail(mut self, p: &mut PispPipeline) -> Option<u32> {
        self.state.progress.fail();
        if let Some((raw, ..)) = &self.raw {
            raw.progress.fail();
        }
        let raw_back = self.release(p);
        match self.raw.as_ref().map(|r| raw_end(r.1, raw_back, None)) {
            Some(RawEnd::Orphan(orphan)) => orphan,
            _ => None,
        }
    }
}

/// What becomes of the buffer of a sliced frame's raw lease when the frame ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawEnd {
    /// The completed frame holds it (`PispFrame::raw_image`): it is the lease's, given back to
    /// the front end when the lease returns (now, if it already has).
    Frame { release_now: bool },
    /// The frame that would have held it never came (or holds another): the front end has it
    /// back. A lease still out returns it later, and that return is ignored (`Some`).
    Orphan(Option<u32>),
}

/// For raw lease buffer `index` (`raw_back`: the lease has returned): the frame completed
/// holding `frame_raw` (`Some`), or failed (`None`).
fn raw_end(index: u32, raw_back: bool, frame_raw: Option<Option<u32>>) -> RawEnd {
    match frame_raw {
        Some(held) if held == Some(index) => RawEnd::Frame {
            release_now: raw_back,
        },
        _ => RawEnd::Orphan((!raw_back).then_some(index)),
    }
}

impl Drop for SlicedOut {
    /// A frame dropped without [`SlicedOut::complete`] or [`SlicedOut::fail`] (a worker that
    /// ended) fails, so nothing waits for it; after either, this changes nothing.
    fn drop(&mut self) {
        self.state.progress.fail();
        if let Some((raw, ..)) = &self.raw {
            raw.progress.fail();
        }
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
    raw_room: bool,
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
        raw_image: s.raw_image.filter(|_| raw_room),
        passes: Vec::new(),
    };
    let state = Arc::new(SliceState {
        progress: Arc::new(FrameProgress::new(s.rows)),
        read: AtomicBool::new(false),
    });
    let Some(lease) = frame_leases(p, &f, buffers, w, returns, Some(state.clone()))? else {
        return Ok(None);
    };
    // Its raw frame, still being written: leased now with a progress of its own, complete
    // with the frame (`SlicedOut::complete`).
    let raw_state = Arc::new(SliceState {
        progress: Arc::new(FrameProgress::new([0, 0])),
        read: AtomicBool::new(false),
    });
    let conversion = super::super::native_backend::native_conversion(w.clock);
    let raw = super::pisp_lease::raw_lease(
        p,
        buffers,
        &f,
        returns,
        &w.live,
        conversion,
        Some(raw_state.clone()),
    );
    let (lease, raw) = match raw {
        Some(r) => {
            let height = r.meta().format.resolution.height.get();
            let lease = lease
                .with_companion(styx_core::prelude::CompanionKind::SensorRaw, r)
                .map_err(|e| format!("raw companion: {e}"))?;
            (lease, Some((raw_state, f.raw_image.unwrap_or(0), height)))
        }
        None => (lease, None),
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
            raw,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw frame leased at a sliced frame's first slice goes back to the front end exactly
    /// once: by its lease when the completed frame holds the buffer, else never (the front end
    /// has it back already).
    #[test]
    fn a_sliced_frames_raw_buffer_goes_back_once() {
        let frame = |release_now| RawEnd::Frame { release_now };
        // Completed, holding the lease's buffer: the lease gives it back (now, if it has).
        assert_eq!(raw_end(3, false, Some(Some(3))), frame(false));
        assert_eq!(raw_end(3, true, Some(Some(3))), frame(true));
        // Completed holding another buffer, or none, or failed: the lease's return is ignored.
        assert_eq!(raw_end(3, false, Some(Some(1))), RawEnd::Orphan(Some(3)));
        assert_eq!(raw_end(3, false, Some(None)), RawEnd::Orphan(Some(3)));
        assert_eq!(raw_end(3, false, None), RawEnd::Orphan(Some(3)));
        assert_eq!(raw_end(3, true, None), RawEnd::Orphan(None));
    }
}
