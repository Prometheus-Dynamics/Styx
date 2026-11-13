//! A dma-buf frame pool for the tests: buffers from the system dma-heap, as the producer of a
//! camera would hand them. Tests skip (printing why) when the heap can't be opened.
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::os::fd::AsFd;

use lemnos_linux_sys::dma_heap;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, packed_layouts};

/// 4096 bytes of frame and 256 of GPU slack.
pub const SIZE: BufferSize = BufferSize::new(4096, 256);

pub fn entry(id: u64) -> FrameEntry {
    let layouts = packed_layouts(FourCc::GREY, 64, 64, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(64, 64).unwrap());
    FrameEntry::new(id, 1_000 + id, format, layouts)
}

/// A pool of `buffers` dma-heap buffers of `SIZE.allocation()` bytes, or `None` when the heap
/// can't be opened here.
pub fn dma_pool(buffers: usize) -> Option<FramePool<()>> {
    let heap = match OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dma_heap/system")
    {
        Ok(heap) => heap,
        Err(err) => {
            println!("skipped: /dev/dma_heap/system is not openable here: {err}");
            return None;
        }
    };
    let len = SIZE.allocation().unwrap();
    let memories: Vec<Box<dyn BufferMemory>> = (0..buffers)
        .map(|_| {
            let fd = dma_heap::allocate(heap.as_fd(), len as u64).expect("a dma-heap buffer");
            Box::new(FdMemory::dmabuf(fd, len, true).expect("mapped")) as Box<dyn BufferMemory>
        })
        .collect();
    Some(FramePool::new(SIZE, memories).expect("a pool"))
}
