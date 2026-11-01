//! V4L2 buffer indices for a dma-buf node where one buffer can be in several queued jobs at
//! once (the raw input of the extra back end passes, the temporal-denoise average they read).
//!
//! V4L2 refuses an index that is already queued, so each buffer keeps its own index (its
//! dma-buf stays attached there) and a few spare indices past them take a buffer again while
//! it is queued; those re-attach whatever buffer they get. Jobs finish in queue order.

use std::collections::VecDeque;

#[derive(Debug)]
pub(super) struct Slots {
    /// Buffers with an index of their own: `0..own`.
    own: u32,
    /// Spare indices: `own..own + spare`.
    spare: u32,
    /// Indices queued and not yet dequeued, oldest first.
    queued: VecDeque<u32>,
}

impl Slots {
    pub(super) fn new(own: u32, spare: u32) -> Self {
        Self {
            own,
            spare,
            queued: VecDeque::with_capacity((own + spare) as usize),
        }
    }

    /// How many indices to request from the node.
    pub(super) fn count(&self) -> u32 {
        self.own + self.spare
    }

    /// The index to queue buffer `buffer` at (its own unless that one is queued), or `None`
    /// when every index it could take is queued.
    pub(super) fn pick(&self, buffer: u32) -> Option<u32> {
        let free = |i: &u32| !self.queued.contains(i);
        std::iter::once(buffer)
            .filter(|&b| b < self.own)
            .chain(self.own..self.own + self.spare)
            .find(free)
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
    fn a_queued_buffer_takes_a_spare_index_and_indices_free_in_order() {
        let mut s = Slots::new(2, 2);
        assert_eq!(s.count(), 4);
        assert_eq!(s.pick(1), Some(1));
        s.queued(1);
        assert_eq!(s.pick(1), Some(2));
        s.queued(2);
        assert_eq!(s.pick(1), Some(3));
        s.queued(3);
        assert_eq!(s.pick(1), None);
        assert_eq!(s.pick(0), Some(0));
        s.dequeued();
        assert_eq!(s.pick(1), Some(1));
        s.clear();
        assert_eq!(s.pick(0), Some(0));
    }
}
