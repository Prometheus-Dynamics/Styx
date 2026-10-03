//! Frame buffers that go back to their pool when the last user drops them: frames are
//! assembled straight into them and handed to applications without another copy.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct Inner {
    free: Mutex<Vec<Vec<u8>>>,
    capacity: usize,
    keep: usize,
    allocations: AtomicU64,
}

/// A pool of frame buffers of one capacity.
#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<Inner>,
}

impl BufferPool {
    /// Buffers of `capacity` bytes; at most `keep` idle ones are kept.
    pub fn new(capacity: usize, keep: usize) -> BufferPool {
        BufferPool {
            inner: Arc::new(Inner {
                free: Mutex::new(Vec::new()),
                capacity,
                keep,
                allocations: AtomicU64::new(0),
            }),
        }
    }

    /// An empty buffer with room for `capacity` bytes, reused when one is idle.
    pub fn take(&self) -> PooledBuffer {
        let reused = self.inner.free.lock().map(|mut f| f.pop()).ok().flatten();
        let buf = reused.unwrap_or_else(|| {
            self.inner.allocations.fetch_add(1, Ordering::Relaxed);
            Vec::with_capacity(self.inner.capacity)
        });
        PooledBuffer {
            buf,
            pool: Some(self.inner.clone()),
        }
    }

    /// The buffers' capacity.
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Buffers allocated so far (the rest were reused).
    pub fn allocations(&self) -> u64 {
        self.inner.allocations.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferPool")
            .field("capacity", &self.inner.capacity)
            .field("allocations", &self.allocations())
            .finish()
    }
}

/// A buffer from a [`BufferPool`].
pub struct PooledBuffer {
    buf: Vec<u8>,
    pool: Option<Arc<Inner>>,
}

impl PooledBuffer {
    /// A buffer that belongs to no pool.
    pub fn detached(buf: Vec<u8>) -> PooledBuffer {
        PooledBuffer { buf, pool: None }
    }
}

impl Deref for PooledBuffer {
    type Target = Vec<u8>;

    fn deref(&self) -> &Vec<u8> {
        &self.buf
    }
}

impl DerefMut for PooledBuffer {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }
}

impl std::fmt::Debug for PooledBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PooledBuffer({} bytes)", self.buf.len())
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        let Some(pool) = self.pool.take() else {
            return;
        };
        let mut buf = std::mem::take(&mut self.buf);
        if buf.capacity() < pool.capacity {
            return;
        }
        buf.clear();
        if let Ok(mut free) = pool.free.lock()
            && free.len() < pool.keep
        {
            free.push(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_come_back() {
        let pool = BufferPool::new(1024, 2);
        let mut a = pool.take();
        a.extend_from_slice(&[1, 2, 3]);
        let ptr = a.as_ptr();
        drop(a);
        let b = pool.take();
        assert!(b.is_empty());
        assert_eq!(b.as_ptr(), ptr);
        assert!(b.capacity() >= 1024);
        assert_eq!(pool.allocations(), 1);
        let (c, d, e) = (pool.take(), pool.take(), pool.take());
        drop((b, c, d, e));
        assert_eq!(pool.allocations(), 4);
        // Only two are kept.
        let _k = (pool.take(), pool.take(), pool.take());
        assert_eq!(pool.allocations(), 5);
    }
}
