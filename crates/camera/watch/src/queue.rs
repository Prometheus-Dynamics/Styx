//! A fixed-capacity FIFO. Its storage is allocated once, at construction.

use std::collections::VecDeque;

/// Holds at most `capacity` items. A push past that is refused and counted.
#[derive(Debug)]
pub(crate) struct Queue<T> {
    items: VecDeque<T>,
    capacity: usize,
    refused: u64,
}

impl<T> Queue<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(capacity),
            capacity,
            refused: 0,
        }
    }

    /// Adds `item`, or counts a refusal when the queue is full.
    pub(crate) fn push(&mut self, item: T) -> bool {
        if self.items.len() >= self.capacity {
            self.refused += 1;
            return false;
        }
        self.items.push_back(item);
        true
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.items.len() >= self.capacity
    }

    /// Events refused because the queue was full. Nonzero means the consumer fell behind.
    pub(crate) fn refused(&self) -> u64 {
        self.refused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_past_capacity_and_counts() {
        let mut q = Queue::with_capacity(2);
        assert!(q.push(1));
        assert!(q.push(2));
        assert!(!q.push(3));
        assert_eq!(q.refused(), 1);
        assert_eq!(q.pop(), Some(1));
        assert!(q.push(4));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(4));
        assert_eq!(q.pop(), None);
    }
}
