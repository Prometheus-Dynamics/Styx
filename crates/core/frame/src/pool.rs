//! The pool: buffers given once, a slot per buffer.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use crate::entry::FrameEntry;
use crate::lease::FrameLease;
use crate::memory::{BufferMemory, CpuAccess, HostMemory};

/// Why [`FramePool::acquire`] gave no buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireError {
    /// Every buffer is held. Nothing was allocated; drop the frame.
    Exhausted,
    /// A plane of the entry ends past the buffer's payload (or its end overflows).
    TooSmall { needed: usize, len: usize },
    /// [`FramePool::acquire_at`]: a lease on buffer `index` is still out.
    Held { index: usize },
    /// [`FramePool::acquire_at`]: the pool has no buffer `index`.
    NoSuchBuffer { index: usize },
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted => f.write_str("every buffer in the pool is held"),
            Self::TooSmall { needed, len } => {
                write!(f, "frame needs {needed} bytes, the buffer has {len}")
            }
            Self::Held { index } => write!(f, "buffer {index} is still held"),
            Self::NoSuchBuffer { index } => write!(f, "the pool has no buffer {index}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for AcquireError {}

/// How big a pool's buffers are: the bytes frames may use, and the slack after them.
///
/// The slack is room past the payload that a GPU reads when it imports the buffer (some GPUs
/// fetch up to a few hundred bytes past the last row, and refuse a dma-buf without it). The pool
/// asks for `payload + gpu_slack` once, when its buffers are made, so a GPU import of any buffer
/// in it is never refused for length. Frames never use the slack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BufferSize {
    /// Bytes a frame may use, from offset 0.
    pub payload: usize,
    /// Bytes past the payload left for a GPU import.
    pub gpu_slack: usize,
}

impl BufferSize {
    pub const fn new(payload: usize, gpu_slack: usize) -> Self {
        Self { payload, gpu_slack }
    }

    /// The bytes each buffer has: payload and slack. `None` on overflow.
    pub const fn allocation(&self) -> Option<usize> {
        self.payload.checked_add(self.gpu_slack)
    }
}

/// Why [`FramePool::new`] refused its buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolError {
    /// Buffer `index` is shorter than [`BufferSize::allocation`].
    BufferTooShort {
        index: usize,
        len: usize,
        needed: usize,
    },
    /// Payload plus slack overflows.
    SizeOverflow,
    /// [`FramePool::register`]: every slot already has a buffer.
    Full { capacity: usize },
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferTooShort { index, len, needed } => write!(
                f,
                "buffer {index} has {len} bytes, the pool needs {needed} (payload and GPU slack)"
            ),
            Self::SizeOverflow => f.write_str("buffer payload plus GPU slack overflows"),
            Self::Full { capacity } => write!(f, "the pool already has its {capacity} buffers"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PoolError {}

/// Counts for a pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub buffers: usize,
    /// Buffers holding a frame now.
    pub in_use: usize,
    pub acquired: usize,
    /// Acquires that found every buffer held.
    pub misses: usize,
}

/// A fixed set of buffers, given once, that frames are written into and handed out from.
///
/// `M` is per-frame metadata kept beside each buffer's [`FrameEntry`], written while one lease
/// holds the frame and read in place by its shares. Cloning the pool clones a handle.
pub struct FramePool<M = ()> {
    pub(crate) shared: Arc<Shared<M>>,
}

pub(crate) struct Shared<M> {
    pub(crate) slots: Box<[Slot<M>]>,
    pub(crate) size: BufferSize,
    /// Slots claimed by `register`.
    registered: AtomicUsize,
    next: AtomicUsize,
    acquired: AtomicUsize,
    misses: AtomicUsize,
}

pub(crate) struct Slot<M> {
    /// The buffer's memory: `None` until it is registered, then never changed.
    memory: UnsafeCell<Option<Box<dyn BufferMemory>>>,
    /// `memory` is written and may be read.
    ready: AtomicBool,
    /// Leases holding the buffer: 0 free, 1 one owner who may write, more: shared, read-only;
    /// [`UNREGISTERED`] before the buffer is given.
    pub(crate) refs: AtomicU32,
    /// A CPU write window is open (begun, not yet ended).
    pub(crate) writing: AtomicBool,
    pub(crate) readers: ReadWindow,
    pub(crate) entry: UnsafeCell<FrameEntry>,
    pub(crate) meta: UnsafeCell<M>,
}

/// `refs` of a slot with no buffer yet: never free, so never acquired.
const UNREGISTERED: u32 = u32::MAX;

impl<M> Slot<M> {
    /// The slot's memory, once registered.
    pub(crate) fn registered(&self) -> Option<&dyn BufferMemory> {
        if !self.ready.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `ready` is set (with release ordering) only after `memory` was written by the
        // one `register` call that claimed this slot, and `memory` is never written again.
        unsafe { (*self.memory.get()).as_deref() }
    }

    /// The memory of a slot a lease holds.
    pub(crate) fn memory(&self) -> &dyn BufferMemory {
        self.registered()
            .unwrap_or_else(|| unreachable!("a lease exists only on a registered buffer"))
    }
}

// SAFETY: `memory` is written once, by the `register` call that claimed the slot, before `ready`
// is set, and only read after. `entry` and `meta` are written only by the one lease holding the slot (refs == 1, through
// `&mut FrameLease`) or by `acquire` after winning the slot from refs == 0; every other access is
// a read while the slot is held, ordered by the acquire/release operations on `refs`. `M` crosses
// threads with the leases, so it must be `Send + Sync`.
unsafe impl<M: Send + Sync> Sync for Slot<M> {}

impl<M: Default> FramePool<M> {
    /// A pool over `memories`, one buffer each, every one at least [`BufferSize::allocation`]
    /// bytes (made by the caller from that size, once). The only allocation the pool makes.
    pub fn new(
        size: BufferSize,
        memories: impl IntoIterator<Item = Box<dyn BufferMemory>>,
    ) -> Result<Self, PoolError> {
        let memories: Vec<Box<dyn BufferMemory>> = memories.into_iter().collect();
        let pool = Self::with_capacity(size, memories.len())?;
        for memory in memories {
            pool.register(memory)?;
        }
        Ok(pool)
    }

    /// A pool with room for `capacity` buffers and none yet: for a producer whose buffers first
    /// appear while it runs (a decoder's output dma-bufs). Each is given once with
    /// [`FramePool::register`]. Every slot is allocated here; registering allocates nothing.
    pub fn with_capacity(size: BufferSize, capacity: usize) -> Result<Self, PoolError> {
        size.allocation().ok_or(PoolError::SizeOverflow)?;
        let slots: Vec<Slot<M>> = (0..capacity)
            .map(|_| Slot {
                memory: UnsafeCell::new(None),
                ready: AtomicBool::new(false),
                refs: AtomicU32::new(UNREGISTERED),
                writing: AtomicBool::new(false),
                readers: ReadWindow::new(),
                entry: UnsafeCell::new(FrameEntry::EMPTY),
                meta: UnsafeCell::new(M::default()),
            })
            .collect();
        Ok(Self {
            shared: Arc::new(Shared {
                slots: slots.into_boxed_slice(),
                size,
                registered: AtomicUsize::new(0),
                next: AtomicUsize::new(0),
                acquired: AtomicUsize::new(0),
                misses: AtomicUsize::new(0),
            }),
        })
    }

    /// `count` heap buffers of `size`, payload and slack.
    pub fn host(count: usize, size: BufferSize) -> Result<Self, PoolError> {
        let len = size.allocation().ok_or(PoolError::SizeOverflow)?;
        Self::new(
            size,
            (0..count).map(|_| Box::new(HostMemory::new(len)) as Box<dyn BufferMemory>),
        )
    }
}

impl<M> FramePool<M> {
    /// A free buffer for a new frame, holding `entry`, as the frame's one lease. Its metadata is
    /// what the buffer's last frame left; set it with [`FrameLease::meta_mut`]. The buffer's bytes
    /// are what its last frame left too: nothing is cleared.
    pub fn acquire(&self, entry: FrameEntry) -> Result<FrameLease<M>, AcquireError> {
        let count = self.shared.slots.len();
        let start = self.shared.next.load(Ordering::Relaxed);
        for step in 0..count {
            let index = (start + step) % count;
            match self.take(index, entry) {
                Err(AcquireError::Held { .. }) => continue,
                Ok(lease) => {
                    self.shared
                        .next
                        .store((index + 1) % count, Ordering::Relaxed);
                    return Ok(lease);
                }
                Err(error) => return Err(error),
            }
        }
        self.shared.misses.fetch_add(1, Ordering::Relaxed);
        Err(AcquireError::Exhausted)
    }

    /// Buffer `index` for a new frame, holding `entry`: for a producer that is told which buffer
    /// a frame landed in (a driver's dequeued buffer, a stream's ring entry). [`AcquireError::Held`]
    /// when a lease on the buffer's last frame is still out: the producer wrote over a frame a
    /// consumer holds, or must drop this one. Counted as a miss.
    pub fn acquire_at(
        &self,
        index: usize,
        entry: FrameEntry,
    ) -> Result<FrameLease<M>, AcquireError> {
        if self.memory(index).is_none() {
            return Err(AcquireError::NoSuchBuffer { index });
        }
        let taken = self.take(index, entry);
        if matches!(taken, Err(AcquireError::Held { .. })) {
            self.shared.misses.fetch_add(1, Ordering::Relaxed);
        }
        taken
    }

    fn take(&self, index: usize, entry: FrameEntry) -> Result<FrameLease<M>, AcquireError> {
        let slot = &self.shared.slots[index];
        if slot
            .refs
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(AcquireError::Held { index });
        }
        let len = self.shared.size.payload;
        let fits = entry.layouts.span().filter(|&needed| needed <= len);
        if fits.is_none() {
            slot.refs.store(0, Ordering::Release);
            return Err(AcquireError::TooSmall {
                needed: entry.layouts.span().unwrap_or(usize::MAX),
                len,
            });
        }
        // SAFETY: this call moved `refs` from 0 to 1, so no lease holds the slot and nothing
        // else reads or writes its entry until a lease for it exists.
        unsafe { *slot.entry.get() = entry };
        self.shared.acquired.fetch_add(1, Ordering::Relaxed);
        Ok(FrameLease::new(self.shared.clone(), index))
    }

    /// Payload and slack of every buffer: what a GPU import is told the buffer's length is
    /// ([`BufferSize::allocation`]).
    pub fn buffer_size(&self) -> BufferSize {
        self.shared.size
    }

    /// Gives the pool one more buffer, once, and returns its index. The buffer must be at least
    /// [`BufferSize::allocation`] bytes. Nothing is allocated: the slot was made with the pool.
    pub fn register(&self, memory: Box<dyn BufferMemory>) -> Result<usize, PoolError> {
        let needed = self
            .shared
            .size
            .allocation()
            .ok_or(PoolError::SizeOverflow)?;
        let capacity = self.shared.slots.len();
        if memory.len() < needed {
            return Err(PoolError::BufferTooShort {
                index: self.shared.registered.load(Ordering::Relaxed).min(capacity),
                len: memory.len(),
                needed,
            });
        }
        let claimed =
            self.shared
                .registered
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < capacity).then_some(n + 1)
                });
        let Ok(index) = claimed else {
            return Err(PoolError::Full { capacity });
        };
        let slot = &self.shared.slots[index];
        // SAFETY: `fetch_update` gave `index` to this call alone, `ready` is still false, so no
        // one reads `memory`, and no other call writes it.
        unsafe { *slot.memory.get() = Some(memory) };
        slot.ready.store(true, Ordering::Release);
        slot.refs.store(0, Ordering::Release);
        Ok(index)
    }

    /// Whether buffer `index` is registered and no lease holds it: a producer checks this before
    /// giving the buffer back to whoever fills it.
    pub fn is_free(&self, index: usize) -> bool {
        self.shared
            .slots
            .get(index)
            .is_some_and(|slot| slot.refs.load(Ordering::Acquire) == 0)
    }

    /// Buffers registered.
    pub fn buffers(&self) -> usize {
        self.shared.registered.load(Ordering::Relaxed)
    }

    /// Buffers the pool has room for.
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Buffer `index`'s memory, for handing the set to a reader once (descriptors, sizes).
    /// `None` when no buffer is registered there.
    pub fn memory(&self, index: usize) -> Option<&dyn BufferMemory> {
        self.shared.slots.get(index)?.registered()
    }

    pub fn stats(&self) -> PoolStats {
        PoolStats {
            buffers: self.buffers(),
            in_use: self
                .shared
                .slots
                .iter()
                .filter(|slot| !matches!(slot.refs.load(Ordering::Relaxed), 0 | UNREGISTERED))
                .count(),
            acquired: self.shared.acquired.load(Ordering::Relaxed),
            misses: self.shared.misses.load(Ordering::Relaxed),
        }
    }
}

impl<M> Clone for FramePool<M> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<M> fmt::Debug for FramePool<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FramePool")
            .field("stats", &self.stats())
            .finish()
    }
}

/// Open CPU reads of one buffer: the sync START when the first opens, END when the last closes.
/// A small spin lock orders the brackets; it is held only around them.
pub(crate) struct ReadWindow {
    lock: AtomicBool,
    open: AtomicU32,
}

impl ReadWindow {
    const fn new() -> Self {
        Self {
            lock: AtomicBool::new(false),
            open: AtomicU32::new(0),
        }
    }

    fn locked<R>(&self, f: impl FnOnce() -> R) -> R {
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        let out = f();
        self.lock.store(false, Ordering::Release);
        out
    }

    pub(crate) fn begin(&self, memory: &dyn BufferMemory) {
        self.locked(|| {
            if self.open.load(Ordering::Relaxed) == 0 {
                memory.begin_cpu(CpuAccess::Read);
            }
            self.open.fetch_add(1, Ordering::Relaxed);
        });
    }

    pub(crate) fn end(&self, memory: &dyn BufferMemory) {
        self.locked(|| {
            if self.open.fetch_sub(1, Ordering::Relaxed) == 1 {
                memory.end_cpu(CpuAccess::Read);
            }
        });
    }
}
