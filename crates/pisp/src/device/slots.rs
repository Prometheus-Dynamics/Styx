//! V4L2 buffer indices for a dma-buf node where one buffer can be in several queued jobs at
//! once (the raw input of the extra back end passes, the temporal-denoise average they read).
//!
//! V4L2 refuses an index that is already queued, so each buffer has a fixed set of indices
//! (`buffer + own * k`, one per job it can be in at once) and is only ever queued at those:
//! an index always holds the same dma-buf, so vb2 keeps its attachment mapped. Spare indices
//! shared by every buffer re-attached (unmapped and mapped through the IOMMU) on every queue:
//! 0.3 ms of kernel time per frame with three passes on the CM5. Jobs finish in queue order.

use std::collections::VecDeque;

#[derive(Debug)]
pub(super) struct Slots {
    /// Buffers: `0..own`.
    own: u32,
    /// Indices per buffer: buffer `b` is queued at `b + own * k`, `k < sets`.
    sets: u32,
    /// Indices queued and not yet dequeued, oldest first.
    queued: VecDeque<u32>,
}

impl Slots {
    /// `own` buffers, each in up to `jobs` queued jobs at once.
    pub(super) fn new(own: u32, jobs: u32) -> Self {
        let sets = jobs.max(1);
        Self {
            own,
            sets,
            queued: VecDeque::with_capacity((own * sets) as usize),
        }
    }

    /// How many indices to request from the node.
    pub(super) fn count(&self) -> u32 {
        self.own * self.sets
    }

    /// The index to queue buffer `buffer` at (the first of its own not queued), or `None` when
    /// all of them are queued.
    pub(super) fn pick(&self, buffer: u32) -> Option<u32> {
        if buffer >= self.own {
            return None;
        }
        (0..self.sets)
            .map(|k| buffer + self.own * k)
            .find(|i| !self.queued.contains(i))
    }

    /// `index` was queued.
    pub(super) fn queued(&mut self, index: u32) {
        self.queued.push_back(index);
    }

    /// The oldest queued index came back.
    pub(super) fn dequeued(&mut self) {
        self.queued.pop_front();
    }

    /// Nothing is queued any more (streaming stopped, or a job failed and its buffers were
    /// dropped).
    pub(super) fn clear(&mut self) {
        self.queued.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::Slots;

    #[test]
    fn a_buffer_only_takes_its_own_indices_and_they_free_in_order() {
        let mut s = Slots::new(2, 3);
        assert_eq!(s.count(), 6);
        assert_eq!(s.pick(1), Some(1));
        s.queued(1);
        assert_eq!(s.pick(1), Some(3));
        s.queued(3);
        assert_eq!(s.pick(1), Some(5));
        s.queued(5);
        assert_eq!(s.pick(1), None);
        assert_eq!(s.pick(0), Some(0));
        assert_eq!(s.pick(2), None);
        s.dequeued();
        assert_eq!(s.pick(1), Some(1));
        s.clear();
        assert_eq!(s.pick(0), Some(0));
    }
}
