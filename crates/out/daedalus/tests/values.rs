//! A Styx frame as a Daedalus value: its key, its dma-buf, and when its buffer goes back.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

mod support;

use std::sync::Arc;

use daedalus::layers::types::{TypeExpr, TypeKey, Typed};
use daedalus_dmabuf::DmaBufBacked;
use std::os::fd::AsRawFd;
use styx_daedalus::{FRAME_KEY, StyxFrame};
use styx_frame::{FramePool, MemoryKind};

#[test]
fn a_frame_is_the_styx_frame_value_and_opaque() {
    assert_eq!(StyxFrame::KEY, TypeKey::new("styx:frame"));
    assert_eq!(FRAME_KEY, "styx:frame");
    assert_eq!(
        StyxFrame::EXPR,
        TypeExpr::Opaque(TypeKey::new("styx:frame"))
    );
}

#[test]
fn a_dma_heap_frame_is_a_dmabuf_of_the_pools_allocation() {
    let Some(pool) = support::dma_pool(2) else {
        return;
    };
    let lease = pool.acquire(support::entry(7)).unwrap();
    let frame = StyxFrame::from_lease(lease, pool.buffer_size()).unwrap();
    assert_eq!(frame.id(), 7);
    assert_eq!(frame.timestamp_ns(), 1_007);
    assert_eq!(frame.memory_kind(), MemoryKind::Dmabuf);
    assert_eq!(frame.import_len(), 4096 + 256, "payload plus GPU slack");
    let (fd, len) = frame.dmabuf().expect("a dma-buf");
    assert_eq!(len, 4096 + 256);
    assert!(fd.as_raw_fd() >= 0);
}

#[test]
fn a_host_frame_is_not_a_dmabuf() {
    let pool: FramePool<()> = FramePool::host(1, support::SIZE).unwrap();
    let lease = pool.acquire(support::entry(1)).unwrap();
    let frame = StyxFrame::from_lease(lease, pool.buffer_size()).unwrap();
    assert_eq!(frame.memory_kind(), MemoryKind::Host);
    assert!(frame.dmabuf().is_none());
}

#[test]
fn a_frame_whose_buffer_is_not_at_the_start_of_its_dmabuf_is_not_expressible() {
    // Several buffers in one dma-buf: the offset is not part of Daedalus's dma-buf value, so the
    // value reports no dma-buf rather than one that starts in the wrong place.
    let Some(heap) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dma_heap/system")
        .ok()
    else {
        return;
    };
    use std::os::fd::AsFd;
    let len = support::SIZE.allocation().unwrap();
    let fd = lemnos_linux_sys::dma_heap::allocate(heap.as_fd(), (2 * len) as u64).unwrap();
    let memory = styx_frame::FdMemory::dmabuf_at(fd, len, len, true).unwrap();
    let pool: FramePool<()> = FramePool::new(
        support::SIZE,
        [Box::new(memory) as Box<dyn styx_frame::BufferMemory>],
    )
    .unwrap();
    let lease = pool.acquire(support::entry(1)).unwrap();
    let frame = StyxFrame::from_lease(lease, pool.buffer_size()).unwrap();
    assert_eq!(frame.memory_kind(), MemoryKind::Dmabuf);
    assert!(frame.dmabuf().is_none());
}

/// Daedalus hands a value on by sharing it: each reference is another hold of the same frame, and
/// the buffer goes back to its pool only when the last one drops.
#[test]
fn the_buffer_goes_back_only_when_the_last_reference_drops() {
    let Some(pool) = support::dma_pool(1) else {
        return;
    };
    let lease = pool.acquire(support::entry(3)).unwrap();
    // A lease share of the same frame, as a second producer of the value would take.
    let share = lease.share();
    let frame = StyxFrame::from_lease(share, pool.buffer_size()).unwrap();
    assert_eq!(frame.holders(), 2, "the caller's lease and the value's");
    // Daedalus's own reference: an Arc clone of the value (no new lease).
    let value = Arc::new(frame);
    let other_reference = Arc::clone(&value);

    drop(lease);
    drop(value);
    assert!(!pool.is_free(0), "a reference is still out");
    assert_eq!(
        pool.acquire(support::entry(4)).err(),
        Some(styx_frame::AcquireError::Exhausted)
    );

    drop(other_reference);
    assert!(pool.is_free(0), "the last reference returned the buffer");
    assert!(pool.acquire(support::entry(5)).is_ok());
}
