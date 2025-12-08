use smallvec::{SmallVec, smallvec};
use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
};

use crate::{format::MediaFormat, metrics::Metrics};

/// External backing for frames when zero-copy sharing external memory.
///
/// Implementations can map DMA buffers or other shared memory into slices.
pub trait ExternalBacking: Send + Sync {
    /// Borrow a plane by index; lifetime must be tied to `self`.
    fn plane_data(&self, index: usize) -> Option<&[u8]>;
}

/// Metadata associated with a frame.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{ColorSpace, FourCc, FrameMeta, MediaFormat, Resolution};
///
/// let res = Resolution::new(640, 480).unwrap();
/// let fmt = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
/// let meta = FrameMeta::new(fmt, 123);
/// assert_eq!(meta.timestamp, 123);
/// ```
#[derive(Debug, Clone)]
pub struct FrameMeta {
    /// Format describing layout and resolution.
    pub format: MediaFormat,
    /// Timestamp in ticks or nanoseconds (caller-defined).
    pub timestamp: u64,
}

impl FrameMeta {
    /// Create metadata with the given format and timestamp.
    pub fn new(format: MediaFormat, timestamp: u64) -> Self {
        Self { format, timestamp }
    }
}

/// Handle to a pooled buffer.
///
/// When dropped, the buffer is returned to the originating pool so downstream
/// stages can reuse memory without reallocations.
///
/// # Example
/// ```rust
/// use styx_core::prelude::BufferPool;
///
/// let pool = BufferPool::with_capacity(2, 1024);
/// let mut lease = pool.lease();
/// lease.resize(16);
/// assert_eq!(lease.len(), 16);
/// ```
pub struct BufferLease {
    pool: Arc<PoolInner>,
    buf: Option<Vec<u8>>,
}

impl BufferLease {
    /// Borrow as an immutable slice.
    pub fn as_slice(&self) -> &[u8] {
        self.buf.as_deref().unwrap_or(&[])
    }

    /// Borrow as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.buf.as_deref_mut().unwrap_or(&mut [])
    }

    /// Current length of the buffer.
    pub fn len(&self) -> usize {
        self.buf.as_ref().map(|b| b.len()).unwrap_or(0)
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ensure the buffer capacity fits `len` bytes and set its length.
    pub fn resize(&mut self, len: usize) {
        if let Some(buf) = self.buf.as_mut() {
            if buf.capacity() < len {
                buf.reserve(len - buf.capacity());
            }
            buf.resize(len, 0);
        }
    }

    /// Set length without initializing; caller must fully write before read.
    ///
    /// # Safety
    /// The buffer contents are uninitialized for any newly exposed bytes.
    pub unsafe fn resize_uninit(&mut self, len: usize) {
        if let Some(buf) = self.buf.as_mut() {
            if buf.capacity() < len {
                buf.reserve(len - buf.capacity());
            }
            unsafe {
                buf.set_len(len);
            }
        }
    }

    /// Replace the leased backing buffer with an owned `Vec<u8>`.
    ///
    /// This is useful for "zero-copy moves" where a stage already owns a `Vec<u8>` and wants to
    /// hand it off as a pooled buffer without another full-frame memcpy.
    pub fn replace_owned(&mut self, buf: Vec<u8>) {
        if let Some(old) = self.buf.take() {
            self.pool.recycle(old);
        }
        self.buf = Some(buf);
    }

    fn take(mut self) -> Vec<u8> {
        self.buf.take().unwrap_or_default()
    }
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            self.pool.recycle(buf);
        }
    }
}

/// Simple buffer pool that hands out reusable owned buffers.
///
/// # Example
/// ```rust
/// use styx_core::prelude::BufferPool;
///
/// let pool = BufferPool::with_limits(4, 1 << 20, 8);
/// let _lease = pool.lease();
/// ```
#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<PoolInner>,
    metrics: Arc<Metrics>,
}

impl BufferPool {
    /// Create a pool with `capacity` preallocated buffers of `chunk_size` bytes.
    pub fn with_capacity(capacity: usize, chunk_size: usize) -> Self {
        Self::with_limits(capacity, chunk_size, capacity)
    }

    /// Create a pool with `capacity` preallocated buffers and a maximum retained free list.
    pub fn with_limits(capacity: usize, chunk_size: usize, max_free: usize) -> Self {
        let mut free = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            free.push(vec![0; chunk_size]);
        }
        Self {
            inner: Arc::new(PoolInner {
                free: Mutex::new(free),
                chunk_size,
                max_free,
            }),
            metrics: Arc::new(Metrics::default()),
        }
    }

    /// Acquire a buffer, allocating if the pool is empty.
    pub fn lease(&self) -> BufferLease {
        let buf = self
            .inner
            .free
            .lock()
            .unwrap()
            .pop()
            .inspect(|_| {
                self.metrics.hit();
            })
            .unwrap_or_else(|| {
                self.metrics.miss();
                self.metrics.alloc();
                vec![0; self.inner.chunk_size]
            });
        BufferLease {
            pool: self.inner.clone(),
            buf: Some(buf),
        }
    }

    /// Access metrics counters for this pool.
    pub fn metrics(&self) -> BufferPoolMetrics {
        BufferPoolMetrics(self.metrics.clone())
    }
}

struct PoolInner {
    free: Mutex<Vec<Vec<u8>>>,
    chunk_size: usize,
    max_free: usize,
}

impl PoolInner {
    fn recycle(&self, mut buf: Vec<u8>) {
        buf.clear();
        let mut free = self.free.lock().unwrap();
        if free.len() < self.max_free {
            free.push(buf);
        }
    }
}

/// Observability for buffer pool behavior.
///
/// # Example
/// ```rust
/// use styx_core::prelude::BufferPool;
///
/// let pool = BufferPool::with_capacity(1, 128);
/// let metrics = pool.metrics();
/// let _ = metrics.hits();
/// ```
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

/// Plane view over a buffer.
///
/// Accessed via `FrameLease::planes`.
#[derive(Debug, Clone, Copy)]
pub struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
}

/// Mutable plane view.
///
/// Accessed via `FrameLease::planes_mut`.
#[derive(Debug)]
pub struct PlaneMut<'a> {
    data: &'a mut [u8],
    stride: usize,
}

impl<'a> Plane<'a> {
    /// Access the raw bytes.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Stride in bytes for this plane.
    pub fn stride(&self) -> usize {
        self.stride
    }
}

impl<'a> PlaneMut<'a> {
    /// Mutable access to plane bytes.
    pub fn data(&mut self) -> &mut [u8] {
        self.data
    }

    /// Stride in bytes for this plane.
    pub fn stride(&self) -> usize {
        self.stride
    }
}

/// Plane layout information stored with a frame.
///
/// # Example
/// ```rust
/// use std::num::NonZeroU32;
/// use styx_core::prelude::plane_layout_from_dims;
///
/// let layout = plane_layout_from_dims(
///     NonZeroU32::new(4).unwrap(),
///     NonZeroU32::new(4).unwrap(),
///     3,
/// );
/// assert_eq!(layout.stride, 12);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct PlaneLayout {
    /// Byte offset into the owning buffer.
    pub offset: usize,
    /// Length of the plane in bytes.
    pub len: usize,
    /// Stride in bytes.
    pub stride: usize,
}

/// Frame container holding one or more planes plus metadata.
///
/// # Example
/// ```rust
/// use styx_core::prelude::*;
///
/// let pool = BufferPool::with_capacity(1, 256);
/// let res = Resolution::new(4, 4).unwrap();
/// let fmt = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
/// let layout = plane_layout_from_dims(res.width, res.height, 3);
/// let meta = FrameMeta::new(fmt, 0);
/// let frame = FrameLease::single_plane(meta, pool.lease(), layout.len, layout.stride);
/// assert_eq!(frame.planes().len(), 1);
/// ```
pub struct FrameLease {
    meta: FrameMeta,
    buffers: SmallVec<[BufferLease; 3]>,
    layouts: SmallVec<[PlaneLayout; 3]>,
    external: Option<Arc<dyn ExternalBacking>>,
}

impl FrameLease {
    /// Construct a single-plane frame using the provided buffer.
    pub fn single_plane(
        meta: FrameMeta,
        mut buffer: BufferLease,
        len: usize,
        stride: usize,
    ) -> Self {
        buffer.resize(len);
        Self {
            meta,
            layouts: smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffers: smallvec![buffer],
            external: None,
        }
    }

    /// Construct a single-plane frame assuming the caller will fully initialize the buffer.
    ///
    /// # Safety
    /// The caller must write every byte of the buffer before the frame is read.
    pub unsafe fn single_plane_uninit(
        meta: FrameMeta,
        mut buffer: BufferLease,
        len: usize,
        stride: usize,
    ) -> Self {
        unsafe {
            buffer.resize_uninit(len);
        }
        Self {
            meta,
            layouts: smallvec![PlaneLayout {
                offset: 0,
                len,
                stride,
            }],
            buffers: smallvec![buffer],
            external: None,
        }
    }

    /// Construct a multi-plane frame from a list of buffers and layouts.
    pub fn multi_plane(
        meta: FrameMeta,
        buffers: SmallVec<[BufferLease; 3]>,
        layouts: SmallVec<[PlaneLayout; 3]>,
    ) -> Self {
        debug_assert_eq!(buffers.len(), layouts.len());
        Self {
            meta,
            buffers,
            layouts,
            external: None,
        }
    }

    /// Construct a frame from an external backing (zero-copy).
    pub fn from_external(
        meta: FrameMeta,
        layouts: SmallVec<[PlaneLayout; 3]>,
        backing: Arc<dyn ExternalBacking>,
    ) -> Self {
        Self {
            meta,
            buffers: SmallVec::new(),
            layouts,
            external: Some(backing),
        }
    }

    /// Metadata describing this frame.
    pub fn meta(&self) -> &FrameMeta {
        &self.meta
    }

    /// Returns true when the frame data is backed by an external zero-copy mapping (e.g. DMA / mmap).
    ///
    /// In this case the frame does not own CPU buffers, so consumers that need an owned `Vec<u8>`
    /// must copy.
    pub fn is_external(&self) -> bool {
        self.external.is_some()
    }

    /// Iterate planes as borrowed slices (zero-copy).
    pub fn planes(&self) -> SmallVec<[Plane<'_>; 3]> {
        if let Some(backing) = &self.external {
            self.layouts
                .iter()
                .enumerate()
                .map(|(idx, layout)| {
                    let slice = backing
                        .plane_data(idx)
                        .map(|s| {
                            let end = layout.offset.saturating_add(layout.len);
                            s.get(layout.offset..end).unwrap_or(&[])
                        })
                        .unwrap_or(&[]);
                    Plane {
                        data: slice,
                        stride: layout.stride,
                    }
                })
                .collect()
        } else {
            self.layouts
                .iter()
                .zip(self.buffers.iter())
                .map(|(layout, buf)| {
                    let slice = buf
                        .as_slice()
                        .get(layout.offset..layout.offset + layout.len)
                        .unwrap_or(&[]);
                    Plane {
                        data: slice,
                        stride: layout.stride,
                    }
                })
                .collect()
        }
    }

    /// Iterate mutable planes for in-place writes.
    pub fn planes_mut(&mut self) -> SmallVec<[PlaneMut<'_>; 3]> {
        if self.external.is_some() {
            return self
                .layouts
                .iter()
                .map(|layout| PlaneMut {
                    data: &mut [],
                    stride: layout.stride,
                })
                .collect();
        }
        self.layouts
            .iter()
            .zip(self.buffers.iter_mut())
            .map(|(layout, buf)| {
                let len = layout.offset + layout.len;
                if buf.len() < len {
                    buf.resize(len);
                }
                let slice = buf
                    .as_mut_slice()
                    .get_mut(layout.offset..layout.offset + layout.len)
                    .unwrap_or(&mut []);
                PlaneMut {
                    data: slice,
                    stride: layout.stride,
                }
            })
            .collect()
    }

    /// Return a copy of plane layouts.
    pub fn layouts(&self) -> SmallVec<[PlaneLayout; 3]> {
        self.layouts.clone()
    }

    /// Return strides for each plane.
    pub fn plane_strides(&self) -> SmallVec<[usize; 3]> {
        self.layouts.iter().map(|l| l.stride).collect()
    }

    /// Convert into owned buffers and metadata.
    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        FrameMeta,
        SmallVec<[PlaneLayout; 3]>,
        SmallVec<[Vec<u8>; 3]>,
    ) {
        let layouts = self.layouts.clone();
        if self.external.is_some() {
            (self.meta, layouts, SmallVec::new())
        } else {
            let buffers = self.buffers.into_iter().map(|lease| lease.take()).collect();
            (self.meta, layouts, buffers)
        }
    }
}

/// Helper for building geometry consistently.
///
/// # Example
/// ```rust
/// use std::num::NonZeroU32;
/// use styx_core::prelude::plane_layout_from_dims;
///
/// let layout = plane_layout_from_dims(
///     NonZeroU32::new(2).unwrap(),
///     NonZeroU32::new(3).unwrap(),
///     4,
/// );
/// assert_eq!(layout.len, 24);
/// ```
pub fn plane_layout_from_dims(
    width: NonZeroU32,
    height: NonZeroU32,
    bytes_per_pixel: usize,
) -> PlaneLayout {
    let stride = width.get() as usize * bytes_per_pixel;
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}

/// Helper to construct a layout when stride is already known.
///
/// # Example
/// ```rust
/// use std::num::NonZeroU32;
/// use styx_core::prelude::plane_layout_with_stride;
///
/// let layout = plane_layout_with_stride(
///     NonZeroU32::new(2).unwrap(),
///     NonZeroU32::new(3).unwrap(),
///     8,
/// );
/// assert_eq!(layout.len, 24);
/// ```
pub fn plane_layout_with_stride(
    _width: NonZeroU32,
    height: NonZeroU32,
    stride: usize,
) -> PlaneLayout {
    let len = stride * height.get() as usize;
    PlaneLayout {
        offset: 0,
        len,
        stride,
    }
}
