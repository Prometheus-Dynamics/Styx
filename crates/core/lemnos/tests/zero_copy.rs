//! Stream buffers become FrameLeases over the memory Lemnos filled: no copy.
//!
//! This runs over a memfd standing in for a stream's shared buffers, without a socket. The
//! simulated backend's stream is not reached from here (see the PR's gaps).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use lemnos_linux_sys::dma_buf;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FdMemory, FrameEntry, FramePool, copy_counters, packed_layouts};
use styx_lemnos::{each_memories, pool_over, whole_memories};

const WIDTH: u32 = 16;
const HEIGHT: u32 = 4;
const BUFFER_BYTES: usize = 64;
const STRIDE: u64 = 4096;

#[test]
fn stream_buffers_become_leases_with_zero_copies() {
    let fd = dma_buf::memfd(c"styx-lemnos-test").expect("memfd");
    let mut file = File::from(fd);
    // Two buffers, each at the ring's stride: buffer 0 holds 0x11, buffer 1 holds 0x22.
    file.set_len(STRIDE * 2).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&[0x11; BUFFER_BYTES]).unwrap();
    file.seek(SeekFrom::Start(STRIDE)).unwrap();
    file.write_all(&[0x22; BUFFER_BYTES]).unwrap();
    let mut readback = [0u8; BUFFER_BYTES];
    file.seek(SeekFrom::Start(STRIDE)).unwrap();
    file.read_exact(&mut readback).unwrap();
    assert_eq!(readback, [0x22; BUFFER_BYTES]);

    let whole = Arc::new(FdMemory::memfd(file.into(), (STRIDE * 2) as usize, true).unwrap());
    let memories = whole_memories(&whole, 2, BUFFER_BYTES).unwrap();
    let pool: FramePool = pool_over(memories, BUFFER_BYTES).unwrap();
    assert_eq!(pool.buffers(), 2);

    let code = FourCc::GREY;
    let format = MediaFormat::with_defaults(code, Resolution::new(WIDTH, HEIGHT).unwrap());
    let layouts = packed_layouts(code, WIDTH, HEIGHT, None, None).unwrap();

    for (buffer, expect) in [(0usize, 0x11u8), (1, 0x22)] {
        let entry = FrameEntry::new(
            1000 + buffer as u64,
            42_000 + buffer as u64,
            format,
            layouts,
        );
        let lease = pool.acquire_at(buffer, entry).expect("acquire");
        assert_eq!(lease.id(), 1000 + buffer as u64);
        assert_eq!(lease.timestamp_ns(), 42_000 + buffer as u64);
        let read = lease.read().expect("cpu-readable mapping");
        let plane = read.plane(0).expect("plane 0");
        assert!(
            plane
                .data
                .iter()
                .take(WIDTH as usize * HEIGHT as usize)
                .all(|&b| b == expect)
        );
    }

    for counter in copy_counters() {
        assert_eq!(counter.copies(), 0, "{} copied", counter.site());
    }
}

#[test]
fn a_dma_buf_per_buffer_maps_the_same_way() {
    let fd = dma_buf::memfd(c"styx-lemnos-each").expect("memfd");
    let file = File::from(fd);
    file.set_len(BUFFER_BYTES as u64).unwrap();
    let each = vec![Arc::new(
        FdMemory::memfd(file.into(), BUFFER_BYTES, false).unwrap(),
    )];
    let memories = each_memories(&each, BUFFER_BYTES).unwrap();
    assert_eq!(memories.len(), 1);
    let _: FramePool = pool_over(memories, BUFFER_BYTES).unwrap();
}
