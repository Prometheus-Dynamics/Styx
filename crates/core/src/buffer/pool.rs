use std::sync::{Arc, Mutex};

use crate::metrics::Metrics;

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

    pub fn lease(&self) -> BufferLease {
        let buf = self
            .inner
            .free
            .lock()
            .unwrap()
            .pop()
            .inspect(|_| self.metrics.hit())
            .unwrap_or_else(|| {
                self.metrics.miss();
                self.metrics.alloc();
                vec![0; self.inner.chunk_size]
            });
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
        let free = self.inner.free.lock().map(|list| list.len()).unwrap_or(0);
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
    pub(super) fn recycle(&self, mut buf: Vec<u8>) {
        buf.clear();
        let mut free = self.free.lock().unwrap();
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
