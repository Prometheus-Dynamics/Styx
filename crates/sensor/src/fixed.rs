//! A small fixed-capacity list: the per-frame results of the scheduler and the driver
//! (landings, mismatches, V4L2 control batches) without allocating.

use core::fmt;
use core::ops::{Deref, DerefMut};

/// Up to `N` values inline, used like a slice (`Deref<Target = [T]>`), compared with slices,
/// arrays and `Vec`s by contents.
#[derive(Clone, Copy)]
pub struct FixedVec<T: Copy + Default, const N: usize> {
    items: [T; N],
    len: usize,
}

impl<T: Copy + Default, const N: usize> FixedVec<T, N> {
    /// An empty list.
    pub fn new() -> Self {
        Self {
            items: [T::default(); N],
            len: 0,
        }
    }

    /// Appends `value`; gives it back when the list is full.
    pub fn push(&mut self, value: T) -> Result<(), T> {
        if self.len == N {
            return Err(value);
        }
        self.items[self.len] = value;
        self.len += 1;
        Ok(())
    }

    /// The capacity.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Copies the values into a `Vec`.
    pub fn to_vec(&self) -> alloc::vec::Vec<T> {
        self.as_slice().to_vec()
    }

    /// The values.
    pub fn as_slice(&self) -> &[T] {
        &self.items[..self.len]
    }

    /// Keeps the values `keep` says yes to, in order.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        let mut n = 0;
        for i in 0..self.len {
            if keep(&self.items[i]) {
                self.items[n] = self.items[i];
                n += 1;
            }
        }
        self.len = n;
    }
}

impl<T: Copy + Default, const N: usize> Default for FixedVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Default, const N: usize> Deref for FixedVec<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Copy + Default, const N: usize> DerefMut for FixedVec<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.items[..self.len]
    }
}

impl<T: Copy + Default + fmt::Debug, const N: usize> fmt::Debug for FixedVec<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: Copy + Default + PartialEq, const N: usize, const M: usize> PartialEq<FixedVec<T, M>>
    for FixedVec<T, N>
{
    fn eq(&self, other: &FixedVec<T, M>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Copy + Default + Eq, const N: usize> Eq for FixedVec<T, N> {}

impl<T: Copy + Default + PartialEq, const N: usize> PartialEq<[T]> for FixedVec<T, N> {
    fn eq(&self, other: &[T]) -> bool {
        self.as_slice() == other
    }
}

impl<T: Copy + Default + PartialEq, const N: usize, const M: usize> PartialEq<[T; M]>
    for FixedVec<T, N>
{
    fn eq(&self, other: &[T; M]) -> bool {
        self.as_slice() == other
    }
}

impl<T: Copy + Default + PartialEq, const N: usize> PartialEq<alloc::vec::Vec<T>>
    for FixedVec<T, N>
{
    fn eq(&self, other: &alloc::vec::Vec<T>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Copy + Default, const N: usize> FromIterator<T> for FixedVec<T, N> {
    /// Collects up to `N` values; the rest are dropped.
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut v = Self::new();
        for x in iter {
            if v.push(x).is_err() {
                break;
            }
        }
        v
    }
}

impl<'a, T: Copy + Default, const N: usize> IntoIterator for &'a FixedVec<T, N> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

/// The values of a [`FixedVec`], by value.
#[derive(Clone, Debug)]
pub struct IntoIter<T: Copy + Default, const N: usize> {
    list: FixedVec<T, N>,
    next: usize,
}

impl<T: Copy + Default, const N: usize> Iterator for IntoIter<T, N> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        let v = self.list.as_slice().get(self.next).copied();
        self.next += 1;
        v
    }
}

impl<T: Copy + Default, const N: usize> IntoIterator for FixedVec<T, N> {
    type Item = T;
    type IntoIter = IntoIter<T, N>;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            list: self,
            next: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behaves_like_a_short_vec() {
        let mut v = FixedVec::<u32, 3>::new();
        assert!(v.is_empty());
        v.push(1).unwrap();
        v.push(2).unwrap();
        v.push(3).unwrap();
        assert_eq!(v.push(4), Err(4));
        assert_eq!(v, [1, 2, 3]);
        assert_eq!(v, alloc::vec![1, 2, 3]);
        assert_eq!(v[1], 2);
        v.retain(|x| *x != 2);
        assert_eq!(v.into_iter().collect::<alloc::vec::Vec<_>>(), [1, 3]);
        let w: FixedVec<u32, 2> = (0..10).collect();
        assert_eq!(w, [0, 1]);
    }
}
