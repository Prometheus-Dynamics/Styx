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
use styx_pipeline::device::{PassOutput, PispFrame, PispPipeline, PispSlice};
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

/// Slice states kept from frame to frame: one whose leases have all returned is reset and
/// handed out again, so a sliced frame allocates no state of its own.
pub(super) struct SliceStates(Vec<Arc<SliceState>>);

/// States kept: more than a sliced frame's three (its own, its passes', its raw frame's)
/// times the frames a consumer holds.
const KEEP_STATES: usize = 16;

impl Default for SliceStates {
    fn default() -> Self {
        Self(Vec::with_capacity(KEEP_STATES))
    }
}

impl SliceStates {
    /// A state of a frame with `rows` final rows, running and unread.
    pub(super) fn take(&mut self, rows: [u32; 2]) -> Arc<SliceState> {
        let free = self.0.iter_mut().position(|s| {
            Arc::get_mut(s).is_some_and(|state| Arc::get_mut(&mut state.progress).is_some())
        });
        if let Some(i) = free {
            let state = Arc::get_mut(&mut self.0[i]).expect("unshared");
            *Arc::get_mut(&mut state.progress).expect("unshared") = FrameProgress::new(rows);
            *state.read.get_mut() = false;
            return self.0[i].clone();
        }
        let state = Arc::new(SliceState {
            progress: Arc::new(FrameProgress::new(rows)),
            read: AtomicBool::new(false),
        });
        if self.0.len() < KEEP_STATES {
            self.0.push(state.clone());
        }
        state
    }
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
    /// Its extra passes' companions, written after it went out: their progress and (output,
    /// buffer) each.
    passes: (Arc<SliceState>, SmallVec<[(usize, u32); 4]>),
}

impl SlicedOut {
    /// Whether `(output, buffer)` is one of the frame's.
    pub(super) fn holds(&self, i: usize, index: u32) -> bool {
        self.buffers.contains(&Some((i, index)))
            || self.passes.1.contains(&(i, index))
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
        // The passes first: the frame's consumers read them once it is complete.
        let (passes, pass_buffers) = &self.passes;
        resync(buffers, passes, pass_buffers.iter().copied());
        passes.progress.complete(self.heights);
        resync(buffers, &self.state, self.buffers.iter().flatten().copied());
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
        self.passes.0.progress.fail();
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

/// Invalidates the caches of `outputs` (cached ones) again if a lease of `state` was read on
/// the CPU before the back end finished writing them.
fn resync(buffers: &Buffers, state: &SliceState, outputs: impl Iterator<Item = (usize, u32)>) {
    if state.read.load(Ordering::Acquire) {
        for (i, index) in outputs {
            if let Some((fd, true)) = buffers.fd(i, index) {
                let _ = dma_heap::sync(fd, Access::Read, true);
            }
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
        self.passes.0.progress.fail();
        self.state.progress.fail();
        if let Some((raw, ..)) = &self.raw {
            raw.progress.fail();
        }
    }
}

/// A sliced frame's first slice, and what leasing it uses.
pub(super) struct Slice<'a> {
    pub(super) s: &'a PispSlice,
    /// Room for its raw frame's lease (`NativeIspConfig::sensor_raw`).
    pub(super) raw_room: bool,
    pub(super) states: &'a mut SliceStates,
    /// Its pass buffers (`PispPipeline::hand_out_passes`), kept from frame to frame.
    pub(super) passes: &'a mut Vec<Option<PassOutput>>,
}

/// The leases of a sliced frame's first slice, and what completes them later. `None` when the
/// frame's sensor values are unknown, or it has no main output (the frame then just arrives
/// whole). Its extra passes run after it goes out: their buffers are taken now and leased with
/// it, complete before it.
pub(super) fn slice_leases(
    p: &mut PispPipeline,
    sl: &mut Slice<'_>,
    buffers: &mut Buffers,
    w: &Worker,
    returns: &Returns,
) -> Result<Option<(FrameLease, SlicedOut)>, String> {
    let s = sl.s;
    let Some(sensor) = p.frame_values(s.sequence) else {
        return Ok(None);
    };
    p.hand_out_passes(s.sequence, sl.passes);
    // The frame as far as it is known: its timestamp (the start of frame), sensor values and
    // buffers; the back end's times come with the frame.
    let mut f = PispFrame {
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
        raw_image: s.raw_image.filter(|_| sl.raw_room),
        passes: std::mem::take(sl.passes),
    };
    let state = sl.states.take(s.rows);
    let pass_state = sl.states.take([0, 0]);
    let progress = Some([state.clone(), pass_state.clone()]);
    let leased = frame_leases(p, &f, buffers, w, returns, progress);
    let pass_buffers = f
        .passes
        .iter()
        .flatten()
        .map(|x| (x.output, x.index))
        .collect();
    *sl.passes = std::mem::take(&mut f.passes);
    // Not handed out (its pass buffers come back with their leases): its passes run as any
    // whole frame's.
    let lease = match leased {
        Ok(Some(lease)) => lease,
        other => {
            p.keep_passes(s.sequence);
            return other.map(|_| None);
        }
    };
    // Its raw frame, still being written: leased now with a progress of its own, complete
    // with the frame (`SlicedOut::complete`).
    let raw_state = sl.states.take([0, 0]);
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
            passes: (pass_state, pass_buffers),
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

    /// A state whose holders are all gone comes back reset, without allocating; one still held
    /// is not reused.
    #[test]
    fn slice_states_are_reused_once_free() {
        let mut states = SliceStates::default();
        // Two made, both used and given back.
        let (a, b) = (states.take([4, 2]), states.take([4, 2]));
        for s in [a, b] {
            s.progress.complete([8, 4]);
            s.read.store(true, Ordering::Relaxed);
        }
        let held = states.take([0, 0]);
        let ((), n) = crate::test_alloc::allocations(|| {
            for _ in 0..100 {
                let s = states.take([4, 2]);
                assert!(!Arc::ptr_eq(&s, &held));
                assert_eq!(s.progress.output(0).rows(), 4);
                assert!(!s.progress.output(0).is_complete());
                assert!(!s.read.load(Ordering::Relaxed));
                s.progress.complete([8, 4]);
                s.read.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(n, 0);
    }
}
