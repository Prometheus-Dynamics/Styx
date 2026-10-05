//! The queue's ring on targets without compare-and-swap (Cortex-M0+, RISC-V without `a`): a
//! `VecDeque` behind the crate's critical-section lock, with the methods of crossbeam's
//! `ArrayQueue` the queue uses (crossbeam's ring needs compare-and-swap on pointers).

use alloc::collections::VecDeque;

use crate::sync::Mutex;

/// A bounded ring: `capacity` values at most.
pub(super) struct ArrayQueue<T> {
    items: Mutex<VecDeque<T>>,
    capacity: usize,
}

impl<T> ArrayQueue<T> {
    /// A ring of `capacity` values (allocated now). Panics on 0, as crossbeam's.
    pub(super) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be non-zero");
        Self {
            items: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Appends `value`, or gives it back when full.
    pub(super) fn push(&self, value: T) -> Result<(), T> {
        let mut items = self.items.lock();
        if items.len() == self.capacity {
            return Err(value);
        }
        items.push_back(value);
        Ok(())
    }

    /// Appends `value`, returning the oldest one when full (dropped by the caller, outside
    /// the critical section).
    pub(super) fn force_push(&self, value: T) -> Option<T> {
        let mut items = self.items.lock();
        let evicted = if items.len() == self.capacity {
            items.pop_front()
        } else {
            None
        };
        items.push_back(value);
        evicted
    }

    /// The oldest value.
    pub(super) fn pop(&self) -> Option<T> {
        self.items.lock().pop_front()
    }

    pub(super) fn len(&self) -> usize {
        self.items.lock().len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.lock().is_empty()
    }

    pub(super) fn capacity(&self) -> usize {
        self.capacity
    }
}

// Built on the host for its tests too (the host has compare-and-swap, so the queue itself uses
// crossbeam's ring there; these check that this one behaves the same).
#[cfg(test)]
mod tests {
    use super::ArrayQueue;

    #[test]
    fn push_refuses_and_force_push_evicts_when_full() {
        let q = ArrayQueue::new(2);
        assert_eq!((q.capacity(), q.len(), q.is_empty()), (2, 0, true));
        assert_eq!(q.push(1), Ok(()));
        assert_eq!(q.push(2), Ok(()));
        assert_eq!(q.push(3), Err(3));
        assert_eq!(q.force_push(4), Some(1));
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.force_push(5), None);
        assert_eq!((q.pop(), q.pop(), q.pop()), (Some(4), Some(5), None));
        assert!(q.is_empty());
    }

    #[test]
    #[should_panic(expected = "capacity must be non-zero")]
    fn zero_capacity_panics() {
        let _ = ArrayQueue::<u8>::new(0);
    }
}
