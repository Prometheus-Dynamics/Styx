//! Share, hold and release order; read and write windows; copies; pool misses.

use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{
    AcquireError, BufferMemory, COPY_INTO, CpuAccess, FrameEntry, FramePool, HostMemory, Layouts,
    MemoryKind, PlaneLayout, SetError, packed_layouts,
};

fn grey(id: u64, width: u32, height: u32) -> FrameEntry {
    let layouts = packed_layouts(FourCc::GREY, width, height, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(width, height).unwrap());
    FrameEntry::new(id, id * 10, format, layouts)
}

#[test]
fn a_buffer_comes_back_only_when_its_last_holder_releases() {
    let pool: FramePool = FramePool::host(2, 64);
    let first = pool.acquire(grey(1, 8, 8)).unwrap();
    assert_eq!(first.holders(), 1);
    let hold_a = first.share();
    let hold_b = first.clone();
    assert_eq!(first.holders(), 3);
    let second = pool.acquire(grey(2, 8, 8)).unwrap();
    assert_ne!(first.buffer_index(), second.buffer_index());
    assert_eq!(
        pool.acquire(grey(3, 8, 8)).unwrap_err(),
        AcquireError::Exhausted
    );

    // The producer lets go first; the holders keep the frame.
    drop(first);
    assert_eq!(
        pool.acquire(grey(3, 8, 8)).unwrap_err(),
        AcquireError::Exhausted
    );
    assert_eq!(hold_a.id(), 1);
    drop(hold_b);
    assert!(hold_a.is_sole_owner());
    drop(hold_a);

    let third = pool.acquire(grey(3, 8, 8)).unwrap();
    assert_eq!(third.id(), 3);
    assert_eq!(pool.stats().misses, 2);
    assert_eq!(pool.stats().in_use, 2);
}

#[test]
fn writes_only_while_sole_owner_and_shares_see_them() {
    let pool: FramePool<u32> = FramePool::host(1, 64);
    let mut frame = pool.acquire(grey(7, 8, 8)).unwrap();
    frame.write().unwrap().plane(0).unwrap().data[5] = 42;
    *frame.meta_mut().unwrap() = 9;
    let share = frame.share();
    assert!(frame.write().is_none());
    assert!(frame.meta_mut().is_none());
    assert_eq!(frame.set_entry(grey(8, 8, 8)), Err(SetError::Shared));
    assert_eq!(share.read().unwrap().plane(0).unwrap().data[5], 42);
    assert_eq!(*share.meta(), 9);
    drop(share);
    // Sole owner again: may write.
    assert!(frame.write().is_some());
    frame.set_entry(grey(8, 8, 8)).unwrap();
    assert_eq!(frame.id(), 8);
}

#[test]
fn reused_buffers_are_not_cleared() {
    let pool: FramePool = FramePool::host(1, 64);
    let mut frame = pool.acquire(grey(1, 8, 8)).unwrap();
    frame.write().unwrap().bytes()[..3].copy_from_slice(&[1, 2, 3]);
    drop(frame);
    let frame = pool.acquire(grey(2, 8, 8)).unwrap();
    assert_eq!(&frame.read().unwrap().bytes()[..3], &[1, 2, 3]);
}

#[test]
fn a_frame_larger_than_its_buffer_is_refused() {
    let pool: FramePool = FramePool::host(1, 63);
    assert_eq!(
        pool.acquire(grey(1, 8, 8)).unwrap_err(),
        AcquireError::TooSmall {
            needed: 64,
            len: 63
        }
    );
    // The buffer is free again.
    assert!(pool.acquire(grey(1, 7, 9)).is_ok());
}

/// Memory that records its sync brackets.
#[derive(Default)]
struct Recording {
    inner: Option<HostMemory>,
    log: Mutex<Vec<(CpuAccess, bool)>>,
    open: AtomicUsize,
}

// SAFETY: forwards the pointer and length of the `HostMemory` it holds.
unsafe impl BufferMemory for Recording {
    fn kind(&self) -> MemoryKind {
        MemoryKind::Dmabuf
    }
    fn len(&self) -> usize {
        self.inner.as_ref().map_or(0, BufferMemory::len)
    }
    fn cpu_ptr(&self) -> Option<NonNull<u8>> {
        self.inner.as_ref()?.cpu_ptr()
    }
    fn cpu_writable(&self) -> bool {
        true
    }
    fn begin_cpu(&self, access: CpuAccess) {
        self.open.fetch_add(1, Ordering::Relaxed);
        self.log.lock().unwrap().push((access, true));
    }
    fn end_cpu(&self, access: CpuAccess) {
        self.open.fetch_sub(1, Ordering::Relaxed);
        self.log.lock().unwrap().push((access, false));
    }
}

#[test]
fn cpu_access_is_bracketed_once_per_window() {
    let memory = Box::new(Recording {
        inner: Some(HostMemory::new(64)),
        ..Default::default()
    });
    let log: *const Recording = &*memory;
    let pool: FramePool = FramePool::new([memory as Box<dyn BufferMemory>]);
    // SAFETY: the pool owns the box and outlives every use of `log` below.
    let recording = unsafe { &*log };

    let mut frame = pool.acquire(grey(1, 8, 8)).unwrap();
    frame.write().unwrap().bytes()[0] = 1;
    frame.write().unwrap().bytes()[1] = 2; // same window
    let share = frame.share(); // ends the write
    {
        let r1 = frame.read().unwrap();
        let r2 = share.read().unwrap(); // overlaps r1: no second START
        assert_eq!(r1.bytes()[1], r2.bytes()[1]);
    }
    drop(share);
    drop(frame);
    use CpuAccess::{Read, Write};
    assert_eq!(
        *recording.log.lock().unwrap(),
        vec![(Write, true), (Write, false), (Read, true), (Read, false)]
    );
    assert_eq!(recording.open.load(Ordering::Relaxed), 0);
}

/// GPU memory: not mapped for the CPU.
struct Gpu;

// SAFETY: gives no pointer.
unsafe impl BufferMemory for Gpu {
    fn kind(&self) -> MemoryKind {
        MemoryKind::Gpu
    }
    fn len(&self) -> usize {
        1 << 20
    }
    fn cpu_ptr(&self) -> Option<NonNull<u8>> {
        None
    }
}

#[test]
fn unmapped_memory_is_handed_on_but_not_read() {
    let pool: FramePool = FramePool::new([Box::new(Gpu) as Box<dyn BufferMemory>]);
    let mut frame = pool.acquire(grey(1, 64, 64)).unwrap();
    assert_eq!(frame.memory_kind(), MemoryKind::Gpu);
    assert!(frame.read().is_none());
    assert!(frame.write().is_none());
    let share = frame.share();
    assert_eq!(share.id(), 1);
}

#[test]
fn copy_into_copies_visible_rows_and_counts_it() {
    // Source rows padded to 16 bytes, destination packed.
    let width = 10u32;
    let height = 4u32;
    let src_layouts = packed_layouts(FourCc::GREY, width, height, Some(16), None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(width, height).unwrap());
    let pool: FramePool = FramePool::host(2, 64);
    let mut src = pool
        .acquire(FrameEntry::new(5, 50, format, src_layouts))
        .unwrap();
    {
        let mut planes = src.write().unwrap();
        let mut plane = planes.plane(0).unwrap();
        for row in 0..height as usize {
            plane
                .row_bytes_mut(row, width as usize)
                .unwrap()
                .fill(row as u8 + 1);
        }
    }
    let dst_layouts = Layouts::new(&[PlaneLayout::new(0, 10, 40)]).unwrap();
    let mut dst = pool
        .acquire(FrameEntry::new(0, 0, format, dst_layouts))
        .unwrap();
    let before = COPY_INTO.copies();
    assert_eq!(src.copy_into(&mut dst).unwrap(), 40);
    assert_eq!(COPY_INTO.copies(), before + 1);
    assert_eq!(dst.id(), 5);
    assert_eq!(dst.timestamp_ns(), 50);
    assert_eq!(*dst.layouts(), dst_layouts);
    let read = dst.read().unwrap();
    let plane = read.plane(0).unwrap();
    for row in 0..4 {
        assert_eq!(plane.row_bytes(row, 10).unwrap(), &[row as u8 + 1; 10]);
    }
}

#[test]
fn leases_move_between_threads() {
    let pool: FramePool<u64> = FramePool::host(2, 64);
    let mut frame = pool.acquire(grey(1, 8, 8)).unwrap();
    *frame.meta_mut().unwrap() = 77;
    let shares: Vec<_> = (0..4).map(|_| frame.share()).collect();
    drop(frame);
    let handles: Vec<_> = shares
        .into_iter()
        .map(|share| {
            std::thread::spawn(move || (*share.meta(), share.read().unwrap().bytes().len()))
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), (77, 64));
    }
    assert_eq!(pool.stats().in_use, 0);
}
