//! memfd and dma-buf memory on Linux.
#![cfg(all(feature = "linux", target_os = "linux"))]

use std::fs::OpenOptions;
use std::os::fd::AsFd;

use lemnos_linux_sys::{dma_buf, udmabuf};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, FdMemory, FrameEntry, FramePool, MemoryKind, packed_layouts};

const LEN: usize = 4096;

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
    let pool: FramePool = FramePool::new([Box::new(memory) as Box<dyn BufferMemory>]);
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
        println!("skipped: /dev/udmabuf is not openable here");
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
    let pool: FramePool = FramePool::new([memory as Box<dyn BufferMemory>]);
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
