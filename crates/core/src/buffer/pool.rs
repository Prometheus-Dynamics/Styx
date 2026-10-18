//! Pooled heap buffers (`no_std` + `alloc`), and with `std` on Linux the memfd pool.

use alloc::vec;
use alloc::vec::Vec;

use crate::metrics::Metrics;
use crate::sync::{Arc, Mutex};

#[cfg(all(feature = "std", target_os = "linux"))]
mod shared;
#[cfg(all(feature = "std", target_os = "linux"))]
pub use shared::{SharedBufferLease, SharedBufferPool, SharedBufferPoolStats};

/// Handle to a pooled buffer.
pub struct BufferLease {
    pub(super) pool: Arc<PoolInner>,
    pub(super) buf: Option<Vec<u8>>,
}

impl BufferLease {
    pub fn as_slice(&self) -> &[u8] {
        self.buf.as_deref().unwrap_or(&[])
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.buf.as_deref_mut().unwrap_or(&mut [])
    }

    pub fn len(&self) -> usize {
        self.buf.as_ref().map(|b| b.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn resize(&mut self, len: usize) {
        if let Some(buf) = self.buf.as_mut() {
            if buf.capacity() < len {
                buf.reserve(len - buf.capacity());
            }
            buf.resize(len, 0);
        }
    }

    /// # Safety
    /// The buffer contents are uninitialized for any newly exposed bytes.
    pub unsafe fn resize_uninit(&mut self, len: usize) {
        if let Some(buf) = self.buf.as_mut() {
            if buf.capacity() < len {
                buf.reserve(len - buf.capacity());
            }
            unsafe { buf.set_len(len) };
        }
    }

    pub fn replace_owned(&mut self, buf: Vec<u8>) {
        if let Some(old) = self.buf.take() {
            self.pool.recycle(old);
        }
        self.buf = Some(buf);
    }

    pub(super) fn take(mut self) -> Vec<u8> {
        self.buf.take().unwrap_or_default()
    }
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        self.pool.metrics.lease_released();
        if let Some(buf) = self.buf.take() {
            self.pool.recycle(buf);
        }
    }
}

/// A pool of heap buffers, reused across leases.
///
/// Reused bytes are unspecified: a buffer from the free list still holds the bytes of its
/// previous lease, and no lease clears them. Leases set the length: `lease` gives the chunk
/// size, `lease_sized` the requested one. Bytes are zero-filled only where a length grows past
/// the reused buffer's, so a caller must write every byte it reads back.
#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<PoolInner>,
    metrics: Arc<Metrics>,
}

#[derive(Clone, Debug)]
pub struct BufferPoolStats {
    pub chunk_size: usize,
    pub free: usize,
    pub free_bytes: usize,
    pub max_free: usize,
    pub retained: usize,
    pub retained_bytes: usize,
    pub in_use: usize,
    pub in_use_bytes: usize,
    pub peak_in_use: usize,
    pub peak_in_use_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub allocations: u64,
}

impl BufferPool {
    pub fn with_capacity(capacity: usize, chunk_size: usize) -> Self {
        Self::with_limits(capacity, chunk_size, capacity)
    }

    pub fn lazy(chunk_size: usize, max_free: usize) -> Self {
        let metrics = Arc::new(Metrics::default());
        Self {
            inner: Arc::new(PoolInner {
                free: Mutex::new(Vec::new()),
                chunk_size,
                max_free,
                metrics: metrics.clone(),
            }),
            metrics,
        }
    }

    pub fn with_limits(capacity: usize, chunk_size: usize, max_free: usize) -> Self {
        let mut free = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            free.push(vec![0; chunk_size]);
        }
        let metrics = Arc::new(Metrics::default());
        Self {
            inner: Arc::new(PoolInner {
                free: Mutex::new(free),
                chunk_size,
                max_free,
                metrics: metrics.clone(),
            }),
            metrics,
        }
    }

    /// A buffer of the chunk size (contents unspecified, see [`BufferPool`]).
    pub fn lease(&self) -> BufferLease {
        // Popped under the lock, resized after it: zero-filling growth is not done under it.
        let reused = self.inner.free.lock().pop();
        let buf = match reused {
            Some(mut buf) => {
                self.metrics.hit();
                buf.resize(self.inner.chunk_size, 0);
                buf
            }
            None => {
                self.metrics.miss();
                self.metrics.alloc();
                vec![0; self.inner.chunk_size]
            }
        };
        self.metrics.lease_acquired();
        BufferLease {
            pool: self.inner.clone(),
            buf: Some(buf),
        }
    }

    /// A buffer of exactly `len` bytes: a free one reused (its length adjusted, zero-filled
    /// only where it grows), else a new allocation of `len` bytes rather than the pool's chunk
    /// size, so a pool serving frames smaller than its chunk (e.g. scaled decodes) does not
    /// allocate and touch the whole chunk. Contents unspecified, see [`BufferPool`].
    pub fn lease_sized(&self, len: usize) -> BufferLease {
        let reused = self.inner.free.lock().pop();
        let buf = match reused {
            Some(mut buf) => {
                self.metrics.hit();
                buf.resize(len, 0);
                buf
            }
            None => {
                self.metrics.miss();
                self.metrics.alloc();
                vec![0; len]
            }
        };
        self.metrics.lease_acquired();
        BufferLease {
            pool: self.inner.clone(),
            buf: Some(buf),
        }
    }

    pub fn metrics(&self) -> BufferPoolMetrics {
        BufferPoolMetrics(self.metrics.clone())
    }

    pub fn stats(&self) -> BufferPoolStats {
        let free = self.inner.free.lock().len();
        let free_bytes = free.saturating_mul(self.inner.chunk_size);
        let in_use = self.metrics.leases_out() as usize;
        let in_use_bytes = in_use.saturating_mul(self.inner.chunk_size);
        let retained = free.saturating_add(in_use);
        let retained_bytes = retained.saturating_mul(self.inner.chunk_size);
        let peak_in_use = self.metrics.peak_leases_out() as usize;
        BufferPoolStats {
            chunk_size: self.inner.chunk_size,
            free,
            free_bytes,
            max_free: self.inner.max_free,
            retained,
            retained_bytes,
            in_use,
            in_use_bytes,
            peak_in_use,
            peak_in_use_bytes: peak_in_use.saturating_mul(self.inner.chunk_size),
            hits: self.metrics.hits(),
            misses: self.metrics.misses(),
            allocations: self.metrics.allocations(),
        }
    }
}

pub(super) struct PoolInner {
    free: Mutex<Vec<Vec<u8>>>,
    chunk_size: usize,
    max_free: usize,
    metrics: Arc<Metrics>,
}

impl PoolInner {
    /// Returns a buffer to the free list with its length and bytes as they are: the next lease
    /// sets the length, zero-filling only growth, so a reused buffer is not cleared.
    pub(super) fn recycle(&self, buf: Vec<u8>) {
        let mut free = self.free.lock();
        if free.len() < self.max_free {
            free.push(buf);
        }
    }
}

#[derive(Clone)]
pub struct BufferPoolMetrics(Arc<Metrics>);

impl BufferPoolMetrics {
    pub fn hits(&self) -> u64 {
        self.0.hits()
    }

    pub fn misses(&self) -> u64 {
        self.0.misses()
    }

    pub fn allocations(&self) -> u64 {
        self.0.allocations()
    }
}

#[cfg(test)]
mod tests {
    use super::BufferPool;

    #[test]
    fn reused_buffers_keep_their_bytes_and_lengths_adjust_without_clearing() {
        let pool = BufferPool::lazy(8, 4);
        let mut lease = pool.lease_sized(8);
        lease
            .as_mut_slice()
            .copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        drop(lease);

        // Reused and truncated: the bytes kept are the previous lease's, not zeros.
        let shrunk = pool.lease_sized(4);
        assert_eq!(shrunk.as_slice(), &[1, 2, 3, 4]);
        drop(shrunk);

        // Reused and grown: only the growth is zero-filled.
        let grown = pool.lease_sized(6);
        assert_eq!(grown.as_slice(), &[1, 2, 3, 4, 0, 0]);
        drop(grown);

        // `lease` gives the chunk size, as a fresh buffer does.
        let chunk = pool.lease();
        assert_eq!(chunk.len(), 8);
        assert_eq!(&chunk.as_slice()[..4], &[1, 2, 3, 4]);
        assert_eq!(pool.stats().hits, 3);
    }
}
