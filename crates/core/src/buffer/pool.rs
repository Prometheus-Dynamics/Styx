use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr::NonNull,
};

#[cfg(target_os = "linux")]
use super::frame::{ExternalBacking, FrameBackingExport, FrameExportError};
use crate::metrics::Metrics;
use parking_lot::Mutex;

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
    pub(super) fn recycle(&self, mut buf: Vec<u8>) {
        buf.clear();
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

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub struct SharedBufferPool {
    inner: Arc<SharedPoolInner>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedBufferPoolStats {
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

#[cfg(target_os = "linux")]
impl SharedBufferPool {
    pub fn with_capacity(capacity: usize, chunk_size: usize) -> Result<Self, FrameExportError> {
        Self::with_limits(capacity, chunk_size, capacity)
    }

    pub fn with_limits(
        capacity: usize,
        chunk_size: usize,
        max_free: usize,
    ) -> Result<Self, FrameExportError> {
        let chunk_size = chunk_size.max(1);
        let inner = Arc::new(SharedPoolInner {
            free: Mutex::new(Vec::with_capacity(capacity)),
            chunk_size,
            max_free,
            metrics: Arc::new(Metrics::default()),
        });
        for _ in 0..capacity {
            inner.recycle(create_sized_memfd(chunk_size)?);
        }
        Ok(Self { inner })
    }

    pub fn lease(&self) -> Result<SharedBufferLease, FrameExportError> {
        let fd = self.inner.free.lock().pop();
        let fd = if let Some(fd) = fd {
            self.inner.metrics.hit();
            fd
        } else {
            self.inner.metrics.miss();
            self.inner.metrics.alloc();
            create_sized_memfd(self.inner.chunk_size)?
        };
        self.inner.metrics.lease_acquired();
        SharedBufferLease::new(self.inner.clone(), fd, self.inner.chunk_size)
    }

    pub fn stats(&self) -> SharedBufferPoolStats {
        let free = self.inner.free.lock().len();
        let free_bytes = free.saturating_mul(self.inner.chunk_size);
        let in_use = self.inner.metrics.leases_out() as usize;
        let in_use_bytes = in_use.saturating_mul(self.inner.chunk_size);
        let retained = free.saturating_add(in_use);
        let retained_bytes = retained.saturating_mul(self.inner.chunk_size);
        let peak_in_use = self.inner.metrics.peak_leases_out() as usize;
        SharedBufferPoolStats {
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
            hits: self.inner.metrics.hits(),
            misses: self.inner.metrics.misses(),
            allocations: self.inner.metrics.allocations(),
        }
    }
}

#[cfg(target_os = "linux")]
struct SharedPoolInner {
    free: Mutex<Vec<OwnedFd>>,
    chunk_size: usize,
    max_free: usize,
    metrics: Arc<Metrics>,
}

#[cfg(target_os = "linux")]
impl SharedPoolInner {
    fn recycle(&self, fd: OwnedFd) {
        let mut free = self.free.lock();
        if free.len() < self.max_free {
            free.push(fd);
        }
    }
}

#[cfg(target_os = "linux")]
pub struct SharedBufferLease {
    pool: Arc<SharedPoolInner>,
    fd: Option<OwnedFd>,
    ptr: Option<NonNull<u8>>,
    len: usize,
    capacity: usize,
}

#[cfg(target_os = "linux")]
impl SharedBufferLease {
    fn new(
        pool: Arc<SharedPoolInner>,
        fd: OwnedFd,
        capacity: usize,
    ) -> Result<Self, FrameExportError> {
        let ptr = map_fd(&fd, capacity, libc::PROT_READ | libc::PROT_WRITE)?;
        Ok(Self {
            pool,
            fd: Some(fd),
            ptr: Some(ptr),
            len: 0,
            capacity,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        let Some(ptr) = self.ptr else {
            return &[];
        };
        unsafe { std::slice::from_raw_parts(ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let Some(ptr) = self.ptr else {
            return &mut [];
        };
        unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn try_resize(&mut self, len: usize) -> Result<(), FrameExportError> {
        if len <= self.capacity {
            self.len = len;
            return Ok(());
        }
        let fd = self
            .fd
            .as_ref()
            .ok_or(FrameExportError::InvalidDescriptor)?;
        if let Some(ptr) = self.ptr.take() {
            unmap_ptr(ptr, self.capacity);
        }
        resize_fd(fd, len)?;
        self.ptr = Some(map_fd(fd, len, libc::PROT_READ | libc::PROT_WRITE)?);
        self.capacity = len;
        self.len = len;
        Ok(())
    }

    pub(super) fn into_external_backing(mut self, plane_count: usize) -> Arc<dyn ExternalBacking> {
        let fd = self.fd.take().expect("shared lease fd missing");
        let ptr = self.ptr.take().expect("shared lease mapping missing");
        Arc::new(SharedBufferBacking {
            pool: self.pool.clone(),
            fd: Some(fd),
            ptr,
            len: self.len,
            capacity: self.capacity,
            plane_count,
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for SharedBufferLease {
    fn drop(&mut self) {
        let release = self.ptr.is_some() || self.fd.is_some();
        if let Some(ptr) = self.ptr.take() {
            unmap_ptr(ptr, self.capacity);
        }
        if let Some(fd) = self.fd.take()
            && self.capacity == self.pool.chunk_size
        {
            self.pool.recycle(fd);
        }
        if release {
            self.pool.metrics.lease_released();
        }
    }
}

#[cfg(target_os = "linux")]
// SAFETY: the lease has unique ownership of the writable mmap and fd while it is live. Moving it
// to another thread does not create aliases, and `Drop` unmaps before recycling the fd.
unsafe impl Send for SharedBufferLease {}

#[cfg(target_os = "linux")]
struct SharedBufferBacking {
    pool: Arc<SharedPoolInner>,
    fd: Option<OwnedFd>,
    ptr: NonNull<u8>,
    len: usize,
    capacity: usize,
    plane_count: usize,
}

#[cfg(target_os = "linux")]
impl ExternalBacking for SharedBufferBacking {
    fn plane_data(&self, index: usize) -> Option<&[u8]> {
        if index >= self.plane_count {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) })
    }

    fn backing_bytes(&self) -> Option<usize> {
        Some(self.len)
    }

    fn backing_kind(&self) -> &'static str {
        "memfd_pool"
    }

    fn residency(&self) -> super::meta::FrameResidency {
        super::meta::FrameResidency::HostExternal
    }

    fn export_backing(&self) -> Result<Option<FrameBackingExport>, FrameExportError> {
        let fd = self
            .fd
            .as_ref()
            .ok_or(FrameExportError::InvalidDescriptor)?;
        Ok(Some(FrameBackingExport::Memfd {
            fd: dup_owned_fd(fd)?,
            len: self.len,
        }))
    }
}

#[cfg(target_os = "linux")]
impl Drop for SharedBufferBacking {
    fn drop(&mut self) {
        unmap_ptr(self.ptr, self.capacity);
        if let Some(fd) = self.fd.take()
            && self.capacity == self.pool.chunk_size
        {
            self.pool.recycle(fd);
        }
        self.pool.metrics.lease_released();
    }
}

#[cfg(target_os = "linux")]
// SAFETY: the backing owns the mmap/fd pair after conversion from `SharedBufferLease`. It exposes
// immutable slices only, unmaps in `Drop`, and recycles the fd only after the backing is dropped.
unsafe impl Send for SharedBufferBacking {}

#[cfg(target_os = "linux")]
// SAFETY: shared access is read-only through `ExternalBacking`; there is no mutation through this
// type after publication, and the mapping remains valid for the backing's lifetime.
unsafe impl Sync for SharedBufferBacking {}

#[cfg(target_os = "linux")]
fn create_sized_memfd(len: usize) -> Result<OwnedFd, FrameExportError> {
    let name = std::ffi::CString::new("styx-shared-buffer").map_err(|err| {
        FrameExportError::Fd(std::io::Error::new(std::io::ErrorKind::InvalidInput, err))
    })?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    resize_fd(&fd, len)?;
    Ok(fd)
}

#[cfg(target_os = "linux")]
fn resize_fd(fd: &OwnedFd, len: usize) -> Result<(), FrameExportError> {
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn map_fd(fd: &OwnedFd, len: usize, prot: i32) -> Result<NonNull<u8>, FrameExportError> {
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(FrameExportError::Mmap(std::io::Error::last_os_error()));
    }
    NonNull::new(addr.cast::<u8>())
        .ok_or_else(|| FrameExportError::Mmap(std::io::Error::other("memfd mmap returned null")))
}

#[cfg(target_os = "linux")]
fn unmap_ptr(ptr: NonNull<u8>, len: usize) {
    unsafe {
        libc::munmap(ptr.as_ptr().cast(), len);
    }
}

#[cfg(target_os = "linux")]
fn dup_owned_fd(fd: &OwnedFd) -> Result<OwnedFd, FrameExportError> {
    let duplicated = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicated < 0 {
        return Err(FrameExportError::Fd(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}
