//! V4L2 buffer indices for a dma-buf node where one buffer can be in several queued jobs at
//! once (the raw input of the extra back end passes, the temporal-denoise average they read).
//!
//! V4L2 refuses an index that is already queued, so each buffer has a fixed set of indices
//! (`buffer + own * k`, one per job it can be in at once) and is only ever queued at those:
//! an index always holds the same dma-buf, so vb2 keeps its attachment mapped. Spare indices
//! shared by every buffer re-attached (unmapped and mapped through the IOMMU) on every queue:
//! 0.3 ms of kernel time per frame with three passes on the CM5. Jobs finish in queue order.
//!
//! A node grants at most `VIDEO_MAX_FRAME` (32) indices, fewer than `own * jobs` for a large
//! input ring, so [`Slots::fit`] gives each buffer as many own indices as fit and keeps spare
//! indices, shared by every buffer, for the jobs beyond them: a buffer can still be in `jobs`
//! queued jobs at once, its extra ones re-attached on each queue.

use std::collections::VecDeque;

#[derive(Debug)]
pub(super) struct Slots {
    /// Buffers: `0..own`.
    own: u32,
    /// Indices per buffer: buffer `b` is queued at `b + own * k`, `k < sets`.
    sets: u32,
    /// All indices: those past `own * sets` are spares any buffer can take.
    count: u32,
    /// Indices queued and not yet dequeued, oldest first.
    queued: VecDeque<u32>,
}

impl Slots {
    /// `own` buffers, each in up to `jobs` queued jobs at once, on `own * jobs` indices.
    pub(super) fn new(own: u32, jobs: u32) -> Self {
        let sets = jobs.max(1);
        Self::with(own, sets, own * sets)
    }

    /// `own` buffers in up to `jobs` queued jobs at once on the `granted` indices a node gave:
    /// the most own indices per buffer that leave a spare for each job beyond them (no more
    /// than `jobs` indices are ever queued at once). `None` when `granted` is under `jobs`.
    pub(super) fn fit(own: u32, jobs: u32, granted: u32) -> Option<Self> {
        let jobs = jobs.max(1);
        if granted < jobs {
            return None;
        }
        let sets = (0..=jobs)
            .rev()
            .find(|&s| own * s + (jobs - s) <= granted)
            .unwrap_or(0);
        Some(Self::with(own, sets, granted.min(own * jobs)))
    }

    fn with(own: u32, sets: u32, count: u32) -> Self {
        Self {
            own,
            sets,
            count,
            queued: VecDeque::with_capacity(count as usize),
        }
    }

    /// How many indices to request from the node.
    pub(super) fn count(&self) -> u32 {
        self.count
    }

    /// The index to queue buffer `buffer` at (the first of its own not queued), or `None` when
    /// all of them are queued.
    pub(super) fn pick(&self, buffer: u32) -> Option<u32> {
        if buffer >= self.own {
            return None;
        }
        (0..self.sets)
            .map(|k| buffer + self.own * k)
            .chain(self.own * self.sets..self.count)
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

    /// The pispbe-input ring of a camera service: ten or more inputs, four jobs, and the 32
    /// indices V4L2 gives at most.
    #[test]
    fn a_large_ring_fits_the_indices_the_node_gives() {
        for own in [1, 2, 8, 10, 12, 16, 31, 32, 40] {
            for granted in 4..=32 {
                let mut s = Slots::fit(own, 4, granted).expect("4 indices are enough");
                assert!(s.count() <= granted, "{own} buffers on {granted}");
                for buffer in 0..own {
                    // Each buffer alone in all four jobs (the config node caps jobs at four).
                    for _ in 0..4 {
                        let i = s.pick(buffer).expect("a free index");
                        assert!(i < s.count());
                        assert!(
                            i % own == buffer || i >= own * s.sets,
                            "{i} is not {buffer}'s"
                        );
                        s.queued(i);
                    }
                    s.clear();
                }
                // Four different buffers queued at once.
                let mut seen = Vec::new();
                for buffer in 0..own.min(4) {
                    let i = s.pick(buffer).unwrap();
                    assert!(!seen.contains(&i));
                    seen.push(i);
                    s.queued(i);
                }
            }
            assert!(Slots::fit(own, 4, 3).is_none());
        }
        // The case that failed: ten inputs, 32 granted: three own indices each, two spares.
        let s = Slots::fit(10, 4, 32).unwrap();
        assert_eq!((s.sets, s.count()), (3, 32));
        // Small rings keep one own index per job.
        assert_eq!(Slots::fit(2, 4, 32).unwrap().sets, 4);
    }
}
