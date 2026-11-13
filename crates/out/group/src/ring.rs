//! A fixed-capacity FIFO. Its slots are allocated once, when it is made.

use alloc::boxed::Box;
use alloc::vec::Vec;

#[derive(Debug)]
pub(crate) struct Ring<T> {
    slots: Box<[Option<T>]>,
    head: usize,
    len: usize,
}

impl<T> Ring<T> {
    /// A ring of `capacity` (at least 1) slots. The only allocation it ever makes.
    pub(crate) fn new(capacity: usize) -> Self {
        let slots: Vec<Option<T>> = (0..capacity.max(1)).map(|_| None).collect();
        Self {
            slots: slots.into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len == self.slots.len()
    }

    fn at(&self, i: usize) -> usize {
        (self.head + i) % self.slots.len()
    }

    /// The `i`-th oldest item.
    pub(crate) fn get(&self, i: usize) -> Option<&T> {
        if i < self.len {
            self.slots[self.at(i)].as_ref()
        } else {
            None
        }
    }

    pub(crate) fn front(&self) -> Option<&T> {
        self.get(0)
    }

    pub(crate) fn back(&self) -> Option<&T> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    /// Appends `item`, or gives it back when the ring is full.
    pub(crate) fn push_back(&mut self, item: T) -> Result<(), T> {
        if self.is_full() {
            return Err(item);
        }
        let at = self.at(self.len);
        self.slots[at] = Some(item);
        self.len += 1;
        Ok(())
    }

    pub(crate) fn pop_front(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let item = self.slots[self.head].take();
        self.head = (self.head + 1) % self.slots.len();
        self.len -= 1;
        item
    }
}
