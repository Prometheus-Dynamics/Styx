//! memfd and dma-buf memory on Linux.
#![cfg(all(feature = "linux", target_os = "linux"))]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

use lemnos_linux_sys::{dma_buf, udmabuf};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{
    BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, MemoryKind, packed_layouts,
};

/// 4096 bytes of frame and 256 of GPU slack, in a page-aligned buffer.
const LEN: usize = 8192;
const SIZE: BufferSize = BufferSize::new(4096, 256);

fn entry(id: u64) -> FrameEntry {
    let layouts = packed_layouts(FourCc::GREY, 64, 64, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(64, 64).unwrap());
    FrameEntry::new(id, 0, format, layouts)
}

#[test]
fn memfd_frames_are_written_and_read_through_the_mapping() {
    let fd = dma_buf::sealed_memfd(c"styx-frame-test", LEN as u64).unwrap();
    let other = fd.try_clone().unwrap();
    let memory = FdMemory::memfd(fd, LEN, true).unwrap();
    let pool: FramePool =
        FramePool::new(SIZE, [Box::new(memory) as Box<dyn BufferMemory>]).unwrap();
    let mut frame = pool.acquire(entry(1)).unwrap();
    assert_eq!(frame.memory_kind(), MemoryKind::Memfd);
    frame.write().unwrap().bytes()[100] = 0xab;
    assert_eq!(frame.read().unwrap().bytes()[100], 0xab);
    // Another mapping of the same descriptor sees it: the bytes are in the memfd.
    let view = lemnos_linux_sys::mmap::Mapping::new(other.as_fd(), LEN, 0, false).unwrap();
    assert_eq!(view.as_slice()[100], 0xab);
    assert!(frame.memory().fd().is_some());
}

#[test]
fn dmabuf_frames_sync_around_cpu_access() {
    let Ok(dev) = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/udmabuf")
    else {
        styx_frame::dma_case::unavailable(
            "styx-frame udmabuf sync",
            "/dev/udmabuf is not openable here",
        );
        return;
    };
    let memfd = udmabuf::memfd_create(c"styx-frame-udmabuf").unwrap();
    std::fs::File::from(memfd.try_clone().unwrap())
        .set_len(LEN as u64)
        .unwrap();
    udmabuf::add_seals(memfd.as_fd(), libc_seal_shrink()).unwrap();
    let dmabuf = udmabuf::create(dev.as_fd(), memfd.as_fd(), 0, LEN as u64).unwrap();
    let memory = Box::new(FdMemory::dmabuf(dmabuf, LEN, true).unwrap());
    let counted: *const FdMemory = &*memory;
    let pool: FramePool = FramePool::new(SIZE, [memory as Box<dyn BufferMemory>]).unwrap();
    // SAFETY: the pool owns the box and outlives every use of `counted` below.
    let counted = unsafe { &*counted };
    let mut frame = pool.acquire(entry(1)).unwrap();
    frame.write().unwrap().bytes()[7] = 9;
    let share = frame.share();
    assert_eq!(share.read().unwrap().bytes()[7], 9);
    drop(share);
    drop(frame);
    let memory = pool.memory(0).unwrap();
    assert_eq!(memory.kind(), MemoryKind::Dmabuf);
    assert_eq!(counted.sync_errors(), 0, "the kernel refused a sync");
    println!("udmabuf: write and read windows synced");
}

fn libc_seal_shrink() -> i32 {
    // F_SEAL_SHRINK
    0x0002
}

#[test]
fn several_buffers_share_one_memfd_at_offsets() {
    // Three 8192-byte buffers in one memfd, the second and third at offsets that are not
    // multiples of the mapping alignment.
    let total = 3 * LEN;
    let fd = dma_buf::sealed_memfd(c"styx-frame-offsets", total as u64).unwrap();
    let memories: Vec<Box<dyn BufferMemory>> = (0..3)
        .map(|i| {
            let part = FdMemory::memfd_at(fd.try_clone().unwrap(), i * LEN, LEN, true).unwrap();
            assert_eq!(part.fd_offset(), i * LEN);
            Box::new(part) as Box<dyn BufferMemory>
        })
        .collect();
    let pool: FramePool = FramePool::new(SIZE, memories).unwrap();
    for i in 0..3u8 {
        let mut frame = pool
            .acquire_at(usize::from(i), entry(u64::from(i)))
            .unwrap();
        frame.write().unwrap().bytes().fill(i + 1);
    }
    // One mapping of the whole descriptor sees each buffer's bytes at its offset, and only there.
    let view = lemnos_linux_sys::mmap::Mapping::new(fd.as_fd(), total, 0, false).unwrap();
    for i in 0..3usize {
        let part = &view.as_slice()[i * LEN..(i + 1) * LEN];
        assert!(part.iter().all(|&b| b == i as u8 + 1), "buffer {i}");
    }
}

/// A dma-buf of `len` bytes whose descriptor is read-only (`O_RDONLY`), from the system heap: what
/// a producer that lends its buffers for reading only would hand over. The error says why there is
/// none, for the test to print.
fn read_only_dmabuf(len: u64) -> Result<OwnedFd, String> {
    /// `struct dma_heap_allocation_data`.
    #[repr(C)]
    struct AllocationData {
        len: u64,
        fd: u32,
        fd_flags: u32,
        heap_flags: u64,
    }
    /// `DMA_HEAP_IOCTL_ALLOC`: `_IOWR('H', 0, struct dma_heap_allocation_data)`.
    const ALLOC: u32 = (3 << 30) | ((size_of::<AllocationData>() as u32) << 16) | (0x48 << 8);
    let path = "/dev/dma_heap/system";
    let heap = File::open(path).map_err(|e| format!("{path} is not openable here: {e}"))?;
    let mut data = AllocationData {
        len,
        fd: 0,
        fd_flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u32,
        heap_flags: 0,
    };
    // SAFETY: FFI. `ALLOC` takes a `dma_heap_allocation_data`, which `data` is (`repr(C)`, the
    // kernel's field order and sizes); the kernel writes only its `fd` field.
    let rc = unsafe { libc::ioctl(heap.as_raw_fd(), ALLOC as _, &raw mut data) };
    if rc < 0 {
        return Err(format!(
            "{path} gives no buffer: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: a successful allocation returns a new dma-buf descriptor no other owner has.
    Ok(unsafe { OwnedFd::from_raw_fd(data.fd as RawFd) })
}

#[test]
fn a_read_only_dmabuf_is_refused_for_writing_and_maps_for_reading() {
    let fd = match read_only_dmabuf(LEN as u64) {
        Ok(fd) => fd,
        Err(why) => {
            styx_frame::dma_case::unavailable("styx-frame read-only dma-buf", &why);
            return;
        }
    };
    // Mapped for writing: refused with its own kind, which `styx-lemnos` hands on as
    // `Error::Map`, so a session that writes its buffers fails to open, typed, not later on a
    // write.
    let err = FdMemory::dmabuf_at(fd.try_clone().unwrap(), 0, LEN, true)
        .expect_err("a read-only descriptor mapped for writing");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");

    // Mapped for reading it is a buffer like any other, and gives no write view.
    let memory = FdMemory::dmabuf_at(fd, 0, LEN, false).expect("mapped for reading");
    let pool: FramePool =
        FramePool::new(SIZE, [Box::new(memory) as Box<dyn BufferMemory>]).unwrap();
    let mut frame = pool.acquire(entry(1)).unwrap();
    assert_eq!(frame.memory_kind(), MemoryKind::Dmabuf);
    assert!(
        frame.write().is_none(),
        "a read-only buffer gave a write view"
    );
    assert!(frame.read().unwrap().bytes().iter().all(|&b| b == 0));
    println!("read-only dma-buf: writable mapping refused ({err}); read-only mapping reads");
}
