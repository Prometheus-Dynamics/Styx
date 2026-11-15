//! The `styx:frame` value: a Styx frame's buffer, held, and its dma-buf.

use std::fmt;
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, Ordering};

use daedalus::layers::types::{TypeExpr, TypeKey, Typed};
use daedalus_dmabuf::{DmaBufBacked, DmaBufMemory};
use styx_frame::{BufferMemory, BufferSize, CpuAccess, FrameLease, MediaFormat, MemoryKind};
use styx_socket::Frame;

/// The type key of a frame: the name of `styx:frame` across a graph.
pub const FRAME_KEY: &str = "styx:frame";

/// What a value holds: the frame's buffer, until the value drops. A pool's lease, or a frame a
/// ring reader holds (the ring's hold goes with it).
trait Hold: Send + Sync {
    fn id(&self) -> u64;
    fn timestamp_ns(&self) -> u64;
    fn format(&self) -> &MediaFormat;
    fn buffer_index(&self) -> usize;
    fn memory(&self) -> &dyn BufferMemory;
    fn holders(&self) -> u32;
}

impl<M: Send + Sync + 'static> Hold for FrameLease<M> {
    fn id(&self) -> u64 {
        FrameLease::id(self)
    }
    fn timestamp_ns(&self) -> u64 {
        FrameLease::timestamp_ns(self)
    }
    fn format(&self) -> &MediaFormat {
        FrameLease::format(self)
    }
    fn buffer_index(&self) -> usize {
        FrameLease::buffer_index(self)
    }
    fn memory(&self) -> &dyn BufferMemory {
        FrameLease::memory(self)
    }
    fn holders(&self) -> u32 {
        FrameLease::holders(self)
    }
}

impl Hold for Frame {
    fn id(&self) -> u64 {
        self.lease().id()
    }
    fn timestamp_ns(&self) -> u64 {
        self.lease().timestamp_ns()
    }
    fn format(&self) -> &MediaFormat {
        self.lease().format()
    }
    fn buffer_index(&self) -> usize {
        self.lease().buffer_index()
    }
    fn memory(&self) -> &dyn BufferMemory {
        self.lease().memory()
    }
    fn holders(&self) -> u32 {
        self.lease().holders()
    }
}

/// A Styx frame as a Daedalus value. Dropping the last holder of the value gives the frame's
/// buffer back: to its pool, or to the ring it was read from.
pub struct StyxFrame {
    hold: Box<dyn Hold>,
    /// The bytes Daedalus may import from the dma-buf: the pool's allocation (payload and GPU
    /// slack), so a device's import of the buffer is never refused for length.
    import_len: u64,
    /// Whether the CPU read bracket of the buffer is open (a dma-buf's sync START). It is opened by
    /// the first [`DmaBufBacked::bytes`] and ended when the frame drops.
    bracket: AtomicBool,
}

impl StyxFrame {
    /// A frame from a pool lease. The import length is the pool's `size.allocation()`. `None` when
    /// that overflows.
    pub fn from_lease<M: Send + Sync + 'static>(
        lease: FrameLease<M>,
        size: BufferSize,
    ) -> Option<Self> {
        let import_len = u64::try_from(size.allocation()?).ok()?;
        Some(Self {
            hold: Box::new(lease),
            import_len,
            bracket: AtomicBool::new(false),
        })
    }

    /// A frame a ring reader holds. The import length is the buffer's own length.
    pub fn from_ring(frame: Frame) -> Self {
        let import_len = frame.lease().memory().len() as u64;
        Self {
            hold: Box::new(frame),
            import_len,
            bracket: AtomicBool::new(false),
        }
    }

    /// The frame's id, as its producer gave it.
    pub fn id(&self) -> u64 {
        self.hold.id()
    }

    /// When the frame was captured, `CLOCK_MONOTONIC` nanoseconds.
    pub fn timestamp_ns(&self) -> u64 {
        self.hold.timestamp_ns()
    }

    /// The frame's format: its FourCC, size and colour. Daedalus never reads it.
    pub fn format(&self) -> &MediaFormat {
        self.hold.format()
    }

    /// Which buffer of its pool holds the frame.
    pub fn buffer_index(&self) -> usize {
        self.hold.buffer_index()
    }

    /// How many leases hold the frame's buffer now: this value's, and any share of it.
    pub fn holders(&self) -> u32 {
        self.hold.holders()
    }

    /// Where the frame's bytes live.
    pub fn memory_kind(&self) -> MemoryKind {
        self.hold.memory().kind()
    }

    /// The length Daedalus imports the dma-buf with.
    pub fn import_len(&self) -> u64 {
        self.import_len
    }
}

impl Drop for StyxFrame {
    fn drop(&mut self) {
        if self.bracket.load(Ordering::Acquire) {
            self.hold.memory().end_cpu(CpuAccess::Read);
        }
    }
}

impl fmt::Debug for StyxFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StyxFrame")
            .field("id", &self.id())
            .field("timestamp_ns", &self.timestamp_ns())
            .field("memory", &self.memory_kind())
            .field("import_len", &self.import_len)
            .finish()
    }
}

impl Typed for StyxFrame {
    const KEY: TypeKey = TypeKey::new(FRAME_KEY);
    // A frame holds a lease, which has no `Typed` impl of its own, so the derive can't describe
    // it. The core never looks inside an opaque value, so this is all Daedalus needs.
    const EXPR: TypeExpr = TypeExpr::Opaque(TypeKey::new(FRAME_KEY));
}

impl DmaBufBacked for StyxFrame {
    /// A Styx frame's fd is read-only where the producer hands it out so (Lemnos does), so the
    /// planner may import it on a device that takes a read-only fd.
    const READ_ONLY: bool = true;

    /// The dma-buf holding the frame, and the import length. `None` for a frame that is not in a
    /// dma-buf (host, memfd, GPU), and for a length the dma-buf can't hold. A frame cut from a
    /// dma-buf at an offset is named here too: the descriptor is the whole dma-buf, and
    /// [`DmaBufBacked::offset`] says where the frame starts in it.
    fn dmabuf(&self) -> Option<(BorrowedFd<'_>, u64)> {
        let memory = self.hold.memory();
        if memory.kind() != MemoryKind::Dmabuf {
            return None;
        }
        if self.import_len > memory.len() as u64 {
            return None;
        }
        Some((memory.fd()?, self.import_len))
    }

    /// Where the frame starts in its dma-buf: the memory's offset in the descriptor. A device that
    /// can bind there imports the frame in place; one that can't copies it from the dma-buf at
    /// this offset, so a misaligned frame still arrives with its own bytes. `u64::MAX` when the
    /// offset doesn't fit a `u64`, which no device can bind at, so the copy fails rather than
    /// reading the wrong bytes.
    fn offset(&self) -> u64 {
        u64::try_from(self.hold.memory().fd_offset()).unwrap_or(u64::MAX)
    }

    /// The frame's bytes, borrowed from the mapping with no copy: the buffer's CPU mapping, from
    /// the frame's own start. Daedalus reads these only for a frame with no dma-buf (host and
    /// memfd frames, and a dma-buf too long for its descriptor): a frame that has a dma-buf is
    /// imported, or copied from it, and is never uploaded from these bytes as well. A dma-buf
    /// read here is under its CPU read bracket, opened here and held until the frame drops, so
    /// the borrow is always inside one. `None` for a buffer that is not mapped here.
    fn bytes(&self) -> Option<&[u8]> {
        let memory = self.hold.memory();
        let ptr = memory.cpu_ptr()?;
        if !self.bracket.swap(true, Ordering::AcqRel) {
            memory.begin_cpu(CpuAccess::Read);
        }
        // SAFETY: `cpu_ptr` is the start of `memory.len()` bytes of this buffer's mapping (Styx's
        // `BufferMemory` contract). The hold owns the buffer and lives as long as `self`, and the
        // borrow is of `self`, so the mapping cannot be unmapped under the slice. The buffer is
        // held by this frame, so the producer does not write it while the frame exists, and the
        // read bracket opened above covers a dma-buf until `Drop` ends it.
        Some(unsafe { core::slice::from_raw_parts(ptr.as_ptr(), memory.len()) })
    }
}

/// The dma-buf memory space, listing the moves of a `styx:frame` between it and the host. Add it
/// to a plan beside the other backends.
pub fn dmabuf_memory() -> DmaBufMemory {
    DmaBufMemory::new().with::<StyxFrame>()
}
