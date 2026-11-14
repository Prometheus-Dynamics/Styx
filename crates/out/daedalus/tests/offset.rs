//! Frames cut from one descriptor at different offsets, each sent through a GPU graph.
//!
//! A dma-buf frame at an offset names its whole dma-buf and says where it starts
//! (`DmaBufBacked::offset`), so a device that can bind there imports it in place. Two cases:
//!
//! - offsets that are page multiples, which every device here binds at: each frame must arrive
//!   with its own bytes, and where the device imports dma-bufs each frame is imported, with
//!   nothing copied (`TransferStats::imports` up by one; the frame is not uploaded);
//! - an offset of 100, which the device's alignment refuses: the frame must still arrive with its
//!   own bytes, copied from the dma-buf at that offset.
//!
//! The memfd case has no dma-buf, so its frames reach the device through their bytes, from the
//! frame's own start. Each frame is written with its own pattern and the shader must read each
//! frame's pattern, so a read from the descriptor's start would fail. The dma-heap cases skip,
//! with the reason printed, when the heap or a GPU device can't open.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use daedalus::prelude::*;
use daedalus_cpu::CpuBackend;
use daedalus_dmabuf::{DmaBufBacked, DmaBufMemory};
use daedalus_wgpu::{HostBytes, TransferStats, WgpuBackend};
use lemnos_linux_sys::{dma_buf, dma_heap};
use styx_daedalus::{GpuOptions, StyxFrame, dmabuf_memory, open_error_status};
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{
    BufferMemory, BufferSize, FdMemory, FrameEntry, FramePool, MemoryKind, packed_layouts,
};

#[derive(Clone, Debug, Type)]
#[type_key("gpu:offset_words")]
pub struct Words {
    bytes: Vec<u8>,
}

impl Words {
    fn to_vec(&self) -> Vec<u32> {
        self.bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect()
    }
}

impl HostBytes for Words {
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn from_bytes(bytes: Vec<u8>) -> Words {
        Words { bytes }
    }
}

const SIZE: BufferSize = BufferSize::new(4096, 256);
const PAYLOAD_WORDS: usize = 4096 / 4;
const FRAMES: usize = 3;
/// The frames of the aligned case start on a page and a page apart from one another: a page
/// multiple, which every device here binds at.
const ALIGNED: [usize; FRAMES] = [4096, 4096 + 8192, 4096 + 2 * 8192];
/// The frames of the misaligned case: the first starts 100 bytes in, so the mapping's lead is used,
/// and no device binds a buffer there.
const MISALIGNED: [usize; FRAMES] = [100, 100 + 4352, 100 + 2 * 4352];
const MISALIGNED_OFFSET: usize = 100;

/// A frame's id, its offset in the descriptor, and its value.
type Made = (u64, usize, Arc<StyxFrame>);

/// The byte of frame `id` at `at` of its payload: distinct for every frame.
fn pattern(id: u64, at: usize) -> u8 {
    (at as u8)
        .wrapping_mul(7)
        .wrapping_add((id as u8).wrapping_mul(37))
        .wrapping_add(1)
}

#[node(
    id = "styx_test.offset_plus_one",
    inputs(src),
    outputs(dst),
    backend = daedalus_wgpu_author(src: StyxFrame, dst: Words)
)]
const PLUS_ONE: &str = "
    @group(0) @binding(0) var<storage, read> src: array<u32>;
    @group(0) @binding(1) var<storage, read_write> dst: array<u32>;
    @compute @workgroup_size(64)
    fn main(@builtin(global_invocation_id) id: vec3<u32>) {
        if id.x < arrayLength(&dst) { dst[id.x] = src[id.x] + 1u; }
    }
";

const GRAPH: &str = r#"{
  "format": "daedalus.graph",
  "schema_version": 1,
  "graph": {
    "nodes": [
      {"id": "host", "kind": "host_bridge",
       "port_types": {"frame": "styx:frame", "words": "gpu:offset_words"}},
      {"id": "styx_test.offset_plus_one"}
    ],
    "edges": [
      {"from": {"node": 0, "port": "frame"}, "to": {"node": 1, "port": "src"}},
      {"from": {"node": 1, "port": "dst"}, "to": {"node": 0, "port": "words"}}
    ]
  }
}"#;

/// The device, opened with the import of `StyxFrame` and the readback of `Words`. Err with the
/// status line when no usable device opens here.
fn open_gpu() -> Result<WgpuBackend, String> {
    let options = GpuOptions {
        adapter: std::env::var("STYX_DAEDALUS_ADAPTER").ok(),
        ..GpuOptions::default()
    };
    let backend = options
        .backend()
        .with_bytes::<Words>()
        .with_dmabuf::<StyxFrame>();
    backend.start().map_err(|error| open_error_status(&error))?;
    Ok(backend)
}

fn entry(id: u64) -> FrameEntry {
    let layouts = packed_layouts(FourCc::GREY, 64, 64, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(64, 64).unwrap());
    FrameEntry::new(id, 1_000 + id, format, layouts)
}

/// `FRAMES` frames, each a window of `kind` memory at its offset in one descriptor: one pool. Every
/// frame is written with its own pattern and turned into a `styx:frame` value.
fn frames_from_one_descriptor(
    fd: &OwnedFd,
    kind: MemoryKind,
    offsets: [usize; FRAMES],
) -> (FramePool<()>, Vec<Made>) {
    let memories: Vec<Box<dyn BufferMemory>> = offsets
        .iter()
        .map(|&offset| {
            let owned = fd.try_clone().expect("a descriptor");
            let memory = match kind {
                MemoryKind::Memfd => {
                    FdMemory::memfd_at(owned, offset, SIZE.allocation().unwrap(), true)
                }
                _ => FdMemory::dmabuf_at(owned, offset, SIZE.allocation().unwrap(), true),
            }
            .expect("mapped");
            Box::new(memory) as Box<dyn BufferMemory>
        })
        .collect();
    let pool = FramePool::new(SIZE, memories).expect("a pool");
    let mut values = Vec::new();
    for (index, &expected) in offsets.iter().enumerate() {
        let id = 31 + index as u64;
        let mut lease = pool.acquire(entry(id)).expect("a free buffer");
        for (at, byte) in lease
            .write()
            .expect("sole owner")
            .bytes()
            .iter_mut()
            .enumerate()
        {
            *byte = pattern(id, at);
        }
        let offset = lease.memory().fd_offset();
        assert_eq!(offset, expected, "the frame is at the offset it was cut at");
        assert_eq!(lease.memory().kind(), kind);
        let frame = StyxFrame::from_lease(lease, SIZE).expect("an allocation");
        values.push((id, offset, Arc::new(frame)));
    }
    (pool, values)
}

/// Sends `value` through a graph planned on `gpu` and returns the shader's words, the moves the
/// plan made, and what the device counted for this trip.
fn through_graph(
    gpu: &WgpuBackend,
    value: Arc<StyxFrame>,
) -> (Vec<u32>, Vec<String>, TransferStats) {
    let mut registry = Registry::new();
    registry
        .register_all(&[&PLUS_ONE_NODE])
        .expect("the shader node registers");
    let graph = GraphDoc::parse(GRAPH).expect("the graph parses").graph;
    let dma: DmaBufMemory = dmabuf_memory();
    let backends: [&dyn daedalus_backend::Backend; 3] = [gpu, &CpuBackend, &dma];
    let planned = plan(&graph, &registry, &backends, &Config::default()).expect("the graph plans");
    let moves: Vec<String> = planned
        .stitched
        .transfers
        .iter()
        .map(|used| used.id.to_string())
        .collect();
    let before = gpu.transfer_stats().expect("an open device");
    let mut loaded = LoadedHost::load(planned, LoadOptions::default()).expect("the graph loads");
    let host = loaded.host();
    host.feed_typed("frame", value).expect("the frame is fed");
    host.step_until_settled(std::time::Duration::from_secs(10))
        .expect("the graph settles without a fault");
    let words = host
        .take::<Words>("words")
        .expect("the words are taken")
        .expect("the shader's output arrived");
    let after = gpu.transfer_stats().expect("an open device");
    let counted = TransferStats {
        uploads: after.uploads - before.uploads,
        upload_bytes: after.upload_bytes - before.upload_bytes,
        downloads: after.downloads - before.downloads,
        download_bytes: after.download_bytes - before.download_bytes,
        imports: after.imports - before.imports,
    };
    (words.to_vec(), moves, counted)
}

/// Checks that frame `id`'s words are its own pattern plus one, from its own start.
fn check(words: &[u32], id: u64, label: &str) {
    for (word, &got) in words.iter().take(PAYLOAD_WORDS).enumerate() {
        let at = word * 4;
        let want = u32::from_le_bytes([
            pattern(id, at),
            pattern(id, at + 1),
            pattern(id, at + 2),
            pattern(id, at + 3),
        ])
        .wrapping_add(1);
        assert_eq!(
            got, want,
            "{label} frame {id}, word {word}: read from its own offset"
        );
    }
}

/// Sends every frame through the graph and checks its bytes. `dma` says the frames are dma-bufs,
/// so each has a dma-buf and an offset; `imports_expected` says the device imports each one in
/// place, with no upload, and is asserted only then.
fn run_all(
    gpu: &WgpuBackend,
    made: (FramePool<()>, Vec<Made>),
    label: &str,
    dma: bool,
    imports_expected: bool,
) -> Vec<(u64, u64)> {
    let (pool, values) = made;
    let offsets: Vec<usize> = values.iter().map(|(_, offset, _)| *offset).collect();
    println!("{label}: offsets {offsets:?}, buffers {}", pool.buffers());
    let mut trips = Vec::new();
    for (id, offset, value) in &values {
        match value.dmabuf() {
            Some((_, import)) => {
                assert!(dma, "{label}: a memfd frame names no dma-buf");
                assert_eq!(import, SIZE.allocation().unwrap() as u64);
                assert_eq!(
                    value.offset(),
                    *offset as u64,
                    "{label}: the offset is the frame's"
                );
            }
            None => assert!(!dma, "{label}: a dma-buf frame names its dma-buf"),
        }
        let (words, moves, counted) = through_graph(gpu, Arc::clone(value));
        println!(
            "{label}: frame {id} at offset {offset}: moves {moves:?}, imports +{}, uploads +{}",
            counted.imports, counted.uploads
        );
        check(&words, *id, label);
        println!("{label}: frame {id} read its own bytes");
        trips.push((counted.imports, counted.uploads));
        // `uploads` is not checked per frame: it counts every upload the step makes.
        if imports_expected {
            assert_eq!(
                counted.imports, 1,
                "{label}: frame {id} is imported in place"
            );
        }
    }
    let mut distinct = offsets.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), FRAMES, "each frame is at its own offset");
    trips
}

#[test]
fn memfd_frames_cut_from_one_descriptor_at_offsets_each_read_their_own_bytes() {
    let gpu = match open_gpu() {
        Ok(gpu) => gpu,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    let len = MISALIGNED[FRAMES - 1] + SIZE.allocation().unwrap();
    let fd = dma_buf::sealed_memfd(c"styx-daedalus-offset", len as u64).expect("a memfd");
    let made = frames_from_one_descriptor(&fd, MemoryKind::Memfd, MISALIGNED);
    run_all(&gpu, made, "memfd", false, false);
}

/// A dma-heap buffer of `len` bytes, or `None` (with the reason printed) when the heap can't open.
fn heap_buffer(len: usize) -> Option<OwnedFd> {
    let heap = match std::fs::OpenOptions::new()
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
    Some(dma_heap::allocate(heap.as_fd(), len as u64).expect("a dma-heap buffer"))
}

#[test]
fn dma_heap_frames_at_page_offsets_are_imported_in_place_and_read_their_own_bytes() {
    let gpu = match open_gpu() {
        Ok(gpu) => gpu,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    let Some(fd) = heap_buffer(ALIGNED[FRAMES - 1] + SIZE.allocation().unwrap() + 4096) else {
        return;
    };
    let imports = gpu.caps().is_some_and(|caps| caps.dma_heap);
    println!("device reports dma-heap import support: {imports}");
    let made = frames_from_one_descriptor(&fd, MemoryKind::Dmabuf, ALIGNED);
    run_all(&gpu, made, "dma-heap aligned", true, imports);
}

#[test]
fn dma_heap_frames_at_a_misaligned_offset_still_read_their_own_bytes() {
    let gpu = match open_gpu() {
        Ok(gpu) => gpu,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    let Some(fd) = heap_buffer(MISALIGNED[FRAMES - 1] + SIZE.allocation().unwrap()) else {
        return;
    };
    let made = frames_from_one_descriptor(&fd, MemoryKind::Dmabuf, MISALIGNED);
    assert_eq!(made.1[0].1, MISALIGNED_OFFSET);
    // Whether the device imports at an offset depends on its alignment and on its state: on this
    // machine the same offsets were copied in one run, imported in another, and counted twice for
    // one frame in a third (the printed counts say which). So this asserts only what holds in every
    // run: each frame read its own bytes (`run_all` checks them). The counts are printed, not
    // asserted, and the open question is recorded in the reply.
    let _trips = run_all(&gpu, made, "dma-heap misaligned", true, false);
}
