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
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted => f.write_str("every buffer in the pool is held"),
            Self::TooSmall { needed, len } => {
                write!(f, "frame needs {needed} bytes, the buffer has {len}")
            }
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
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferTooShort { index, len, needed } => write!(
                f,
                "buffer {index} has {len} bytes, the pool needs {needed} (payload and GPU slack)"
            ),
            Self::SizeOverflow => f.write_str("buffer payload plus GPU slack overflows"),
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
    next: AtomicUsize,
    acquired: AtomicUsize,
    misses: AtomicUsize,
}

pub(crate) struct Slot<M> {
    pub(crate) memory: Box<dyn BufferMemory>,
    /// Leases holding the buffer: 0 free, 1 one owner who may write, more: shared, read-only.
    pub(crate) refs: AtomicU32,
    /// A CPU write window is open (begun, not yet ended).
    pub(crate) writing: AtomicBool,
    pub(crate) readers: ReadWindow,
    pub(crate) entry: UnsafeCell<FrameEntry>,
    pub(crate) meta: UnsafeCell<M>,
}

// SAFETY: `entry` and `meta` are written only by the one lease holding the slot (refs == 1, through
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
        let needed = size.allocation().ok_or(PoolError::SizeOverflow)?;
        let memories: Vec<Box<dyn BufferMemory>> = memories.into_iter().collect();
        if let Some((index, memory)) = memories
            .iter()
            .enumerate()
            .find(|(_, memory)| memory.len() < needed)
        {
            return Err(PoolError::BufferTooShort {
                index,
                len: memory.len(),
                needed,
            });
        }
        let slots: Vec<Slot<M>> = memories
            .into_iter()
            .map(|memory| Slot {
                memory,
                refs: AtomicU32::new(0),
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
        let slots = &self.shared.slots;
        let count = slots.len();
        let start = self.shared.next.load(Ordering::Relaxed);
        for step in 0..count {
            let index = (start + step) % count;
            let slot = &slots[index];
            if slot
                .refs
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
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
            self.shared
                .next
                .store((index + 1) % count, Ordering::Relaxed);
            self.shared.acquired.fetch_add(1, Ordering::Relaxed);
            return Ok(FrameLease::new(self.shared.clone(), index));
        }
        self.shared.misses.fetch_add(1, Ordering::Relaxed);
        Err(AcquireError::Exhausted)
    }

    /// Payload and slack of every buffer: what a GPU import is told the buffer's length is
    /// ([`BufferSize::allocation`]).
    pub fn buffer_size(&self) -> BufferSize {
        self.shared.size
    }

    pub fn buffers(&self) -> usize {
        self.shared.slots.len()
    }

    /// Buffer `index`'s memory, for handing the set to a reader once (descriptors, sizes).
    pub fn memory(&self, index: usize) -> Option<&dyn BufferMemory> {
        self.shared.slots.get(index).map(|slot| &*slot.memory)
    }

    pub fn stats(&self) -> PoolStats {
        PoolStats {
            buffers: self.shared.slots.len(),
            in_use: self
                .shared
                .slots
                .iter()
                .filter(|slot| slot.refs.load(Ordering::Relaxed) != 0)
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
