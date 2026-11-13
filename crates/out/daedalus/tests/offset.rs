//! Frames cut from one descriptor at different offsets, each sent through a GPU graph.
//!
//! A dma-buf frame at a nonzero offset has no dma-buf for Daedalus (its `DmaBufBacked::dmabuf`
//! names the whole descriptor, from zero), so it reaches the device through its bytes: the
//! mapping from the frame's own start. Each frame is written with its own pattern, and the shader
//! must read each frame's pattern, so a read from the descriptor's start would fail. The memfd
//! case always runs; the dma-heap case skips, with the reason printed, when the heap can't open.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use daedalus::prelude::*;
use daedalus_cpu::CpuBackend;
use daedalus_dmabuf::{DmaBufBacked, DmaBufMemory};
use daedalus_wgpu::{HostBytes, Options, WgpuBackend};
use lemnos_linux_sys::{dma_buf, dma_heap};
use styx_daedalus::{StyxFrame, dmabuf_memory};
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
/// The first frame starts here, not on a page or 64 KiB boundary, so the mapping's lead is used.
const FIRST_OFFSET: usize = 100;
const FRAMES: usize = 3;
const STRIDE: usize = 4352;

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

fn open_gpu() -> Result<WgpuBackend, String> {
    let options = Options {
        adapter: std::env::var("STYX_DAEDALUS_ADAPTER").ok(),
        ..Options::default()
    };
    let backend = WgpuBackend::open(options)
        .with_bytes::<Words>()
        .with_dmabuf::<StyxFrame>();
    backend
        .start()
        .map_err(|error| format!("no usable GPU device: {error}"))?;
    Ok(backend)
}

fn entry(id: u64) -> FrameEntry {
    let layouts = packed_layouts(FourCc::GREY, 64, 64, None, None).unwrap();
    let format = MediaFormat::with_defaults(FourCc::GREY, Resolution::new(64, 64).unwrap());
    FrameEntry::new(id, 1_000 + id, format, layouts)
}

/// Frames `FRAMES` of them, each a window of `memory_of(index)` at its offset: one pool, one
/// descriptor. Every frame is written with its own pattern and turned into a `styx:frame` value.
fn frames_from_one_descriptor(fd: &OwnedFd, kind: MemoryKind) -> (FramePool<()>, Vec<Made>) {
    let memories: Vec<Box<dyn BufferMemory>> = (0..FRAMES)
        .map(|index| {
            let offset = FIRST_OFFSET + index * STRIDE;
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
    for index in 0..FRAMES {
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
        assert_eq!(lease.memory().kind(), kind);
        let frame = StyxFrame::from_lease(lease, SIZE).expect("an allocation");
        values.push((id, offset, Arc::new(frame)));
    }
    (pool, values)
}

/// Sends `value` through the graph and returns the shader's words.
fn through_graph(gpu: &WgpuBackend, value: Arc<StyxFrame>) -> (Vec<u32>, Vec<String>) {
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
    let mut loaded = LoadedHost::load(planned, LoadOptions::default()).expect("the graph loads");
    let host = loaded.host();
    host.feed_typed("frame", value).expect("the frame is fed");
    host.step_until_settled(std::time::Duration::from_secs(10))
        .expect("the graph settles without a fault");
    let words = host
        .take::<Words>("words")
        .expect("the words are taken")
        .expect("the shader's output arrived");
    (words.to_vec(), moves)
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

fn run_all(gpu: &WgpuBackend, pool_and_values: (FramePool<()>, Vec<Made>), label: &str) {
    let (pool, values) = pool_and_values;
    let offsets: Vec<usize> = values.iter().map(|(_, offset, _)| *offset).collect();
    println!("{label}: offsets {offsets:?}, buffers {}", pool.buffers());
    for (id, offset, value) in &values {
        assert!(
            value.dmabuf().is_none(),
            "a frame at an offset names no dma-buf"
        );
        let (words, moves) = through_graph(gpu, Arc::clone(value));
        println!("{label}: frame {id} at offset {offset}: moves {moves:?}");
        check(&words, *id, label);
        println!("{label}: frame {id} read its own bytes");
    }
    assert_eq!(offsets.len(), FRAMES);
    let mut distinct = offsets.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), FRAMES, "each frame is at its own offset");
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
    let len = FIRST_OFFSET + FRAMES * STRIDE;
    let fd = dma_buf::sealed_memfd(c"styx-daedalus-offset", len as u64).expect("a memfd");
    let made = frames_from_one_descriptor(&fd, MemoryKind::Memfd);
    run_all(&gpu, made, "memfd");
}

#[test]
fn dma_heap_frames_cut_from_one_dma_buf_at_offsets_each_read_their_own_bytes() {
    let heap = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dma_heap/system")
    {
        Ok(heap) => heap,
        Err(err) => {
            println!("skipped: /dev/dma_heap/system is not openable here: {err}");
            return;
        }
    };
    let gpu = match open_gpu() {
        Ok(gpu) => gpu,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    let len = FIRST_OFFSET + FRAMES * STRIDE;
    let fd = dma_heap::allocate(heap.as_fd(), len as u64).expect("a dma-heap buffer");
    let made = frames_from_one_descriptor(&fd, MemoryKind::Dmabuf);
    run_all(&gpu, made, "dma-heap");
}
