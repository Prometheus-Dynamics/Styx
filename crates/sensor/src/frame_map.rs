//! A small map from frame numbers to values with a fixed capacity: the control schedule's
//! history and pending requests without allocating per frame.

/// Up to `N` `(frame, value)` entries sorted by frame. When full, inserting a new frame drops
/// the oldest entry there (counted in [`Self::dropped`]).
#[derive(Debug, Clone)]
pub(crate) struct FrameMap<V: Copy + Default, const N: usize> {
    keys: [u64; N],
    values: [V; N],
    len: usize,
    dropped: u64,
}

impl<V: Copy + Default, const N: usize> Default for FrameMap<V, N> {
    fn default() -> Self {
        Self {
            keys: [0; N],
            values: [V::default(); N],
            len: 0,
            dropped: 0,
        }
    }
}

impl<V: Copy + Default, const N: usize> FrameMap<V, N> {
    fn find(&self, frame: u64) -> Result<usize, usize> {
        self.keys[..self.len].binary_search(&frame)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Entries dropped because the map was full.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Sets the value of `frame` (replacing one there).
    pub(crate) fn insert(&mut self, frame: u64, value: V) {
        *self.entry(frame) = value;
    }

    /// The value of `frame`, inserted as `V::default()` if there is none.
    pub(crate) fn entry(&mut self, frame: u64) -> &mut V {
        let mut i = match self.find(frame) {
            Ok(i) => return &mut self.values[i],
            Err(i) => i,
        };
        if self.len == N {
            // Full: the oldest entry there goes; the new one is kept.
            self.dropped += 1;
            self.remove_front(1);
            i = i.saturating_sub(1);
        }
        self.keys.copy_within(i..self.len, i + 1);
        self.values.copy_within(i..self.len, i + 1);
        self.keys[i] = frame;
        self.values[i] = V::default();
        self.len += 1;
        &mut self.values[i]
    }

    /// The value of `frame`.
    pub(crate) fn get(&self, frame: u64) -> Option<&V> {
        self.find(frame).ok().map(|i| &self.values[i])
    }

    /// Entries in frame order.
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = (u64, &V)> + '_ {
        self.keys[..self.len]
            .iter()
            .copied()
            .zip(self.values[..self.len].iter())
    }

    /// Entries up to and including `frame`, in frame order.
    pub(crate) fn up_to(&self, frame: u64) -> impl DoubleEndedIterator<Item = (u64, &V)> + '_ {
        let end = match self.find(frame) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        self.keys[..end]
            .iter()
            .copied()
            .zip(self.values[..end].iter())
    }

    /// The last entry at or before `frame`.
    pub(crate) fn last_at_or_before(&self, frame: u64) -> Option<(u64, V)> {
        self.up_to(frame).next_back().map(|(k, v)| (k, *v))
    }

    fn remove_front(&mut self, n: usize) {
        self.keys.copy_within(n..self.len, 0);
        self.values.copy_within(n..self.len, 0);
        self.len -= n;
    }

    /// Removes the entries before `frame`.
    pub(crate) fn remove_before(&mut self, frame: u64) {
        let n = match self.find(frame) {
            Ok(i) | Err(i) => i,
        };
        self.remove_front(n);
    }

    /// Removes the entries up to and including `frame`; returns the value of the last of them.
    pub(crate) fn take_up_to(&mut self, frame: u64) -> Option<V> {
        let end = match self.find(frame) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        let last = end.checked_sub(1).map(|i| self.values[i]);
        self.remove_front(end);
        last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order_replaces_and_drops_the_oldest_when_full() {
        let mut m = FrameMap::<u32, 4>::default();
        for (k, v) in [(5, 50), (1, 10), (3, 30), (3, 31)] {
            m.insert(k, v);
        }
        assert_eq!(
            m.iter().map(|(k, v)| (k, *v)).collect::<Vec<_>>(),
            [(1, 10), (3, 31), (5, 50)]
        );
        assert_eq!(m.last_at_or_before(4), Some((3, 31)));
        assert_eq!(m.last_at_or_before(0), None);
        m.insert(9, 90);
        m.insert(7, 70); // full: frame 1 goes
        assert_eq!(m.dropped(), 1);
        assert_eq!(m.iter().map(|(k, _)| k).collect::<Vec<_>>(), [3, 5, 7, 9]);
        m.insert(0, 1); // older than everything in a full map: frame 3 goes, 0 is kept
        assert_eq!(m.get(0), Some(&1));
        assert_eq!(m.iter().map(|(k, _)| k).collect::<Vec<_>>(), [0, 5, 7, 9]);
        assert_eq!(m.take_up_to(6), Some(50));
        assert_eq!(m.iter().map(|(k, _)| k).collect::<Vec<_>>(), [7, 9]);
        m.remove_before(9);
        assert_eq!(
            m.iter().map(|(k, v)| (k, *v)).collect::<Vec<_>>(),
            [(9, 90)]
        );
        assert_eq!(m.take_up_to(8), None);
        *m.entry(9) += 1;
        assert_eq!(m.get(9), Some(&91));
    }
}
