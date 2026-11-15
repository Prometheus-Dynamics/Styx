//! A producer that cycles through more dma-buf buffers than a graph keeps imports for.
//!
//! Four dma-heap buffers come round in turn, three times, into one loaded graph. With the default
//! bound (64) each buffer is imported once and found again when it comes round: four imports in
//! all. With a bound of 2, which is smaller than the cycle, each frame is imported again: twelve.
//! Either way every frame must arrive with its own bytes: the kept import is an optimisation, and a
//! smaller bound costs imports and nothing else. Each run uses a device of its own (a named adapter),
//! since a device shared by the process keeps the bound it was opened with.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

use std::sync::Arc;

use daedalus::prelude::*;
use daedalus_cpu::CpuBackend;
use daedalus_dmabuf::DmaBufMemory;
use daedalus_wgpu::{HostBytes, WgpuBackend};
use styx_daedalus::{GpuOptions, StyxFrame, dmabuf_memory, open_error_status};

mod support;

#[derive(Clone, Debug, Type)]
#[type_key("gpu:kept_words")]
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

const BUFFERS: usize = 4;
const ROUNDS: usize = 3;
const PAYLOAD_WORDS: usize = 4096 / 4;

/// The byte at `at` of the frame written in `round` into `buffer`: distinct for every frame.
fn pattern(round: usize, buffer: usize, at: usize) -> u8 {
    (at as u8)
        .wrapping_mul(7)
        .wrapping_add((round as u8).wrapping_mul(53))
        .wrapping_add((buffer as u8).wrapping_mul(29))
        .wrapping_add(1)
}

#[node(
    id = "styx_test.kept_plus_one",
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
       "port_types": {"frame": "styx:frame", "words": "gpu:kept_words"}},
      {"id": "styx_test.kept_plus_one"}
    ],
    "edges": [
      {"from": {"node": 0, "port": "frame"}, "to": {"node": 1, "port": "src"}},
      {"from": {"node": 1, "port": "dst"}, "to": {"node": 0, "port": "words"}}
    ]
  }
}"#;

/// A device of this run's own, with `kept_imports` as its bound. The process's shared device keeps
/// the bound of whichever backend opened it, so the device is named, and so private. Err with the
/// status line when no usable device opens here.
fn open_private(kept_imports: usize) -> Result<WgpuBackend, String> {
    let shared = GpuOptions::default()
        .backend()
        .start()
        .map_err(|error| open_error_status(&error))?;
    let backend = GpuOptions {
        adapter: Some(shared.adapter.name.clone()),
        kept_imports,
    }
    .backend()
    .with_bytes::<Words>()
    .with_dmabuf::<StyxFrame>();
    backend.start().map_err(|error| open_error_status(&error))?;
    Ok(backend)
}

/// Cycles the buffers through one loaded graph and checks every frame. Returns how many imports
/// and uploads the device counted over the cycle, or `None` when the device or heap can't open.
fn cycle(kept_imports: usize) -> Option<(u64, u64)> {
    let gpu = match open_private(kept_imports) {
        Ok(gpu) => gpu,
        Err(why) => {
            println!("skipped: {why}");
            return None;
        }
    };
    let pool = support::dma_pool(BUFFERS)?;
    let mut registry = Registry::new();
    registry
        .register_all(&[&PLUS_ONE_NODE])
        .expect("the shader node registers");
    let graph = GraphDoc::parse(GRAPH).expect("the graph parses").graph;
    let dma: DmaBufMemory = dmabuf_memory();
    let backends: [&dyn daedalus_backend::Backend; 3] = [&gpu, &CpuBackend, &dma];
    let planned = plan(&graph, &registry, &backends, &Config::default()).expect("the graph plans");
    let mut loaded = LoadedHost::load(planned, LoadOptions::default()).expect("the graph loads");
    let host = loaded.host();
    let before = gpu.transfer_stats().expect("an open device");
    for round in 0..ROUNDS {
        for buffer in 0..BUFFERS {
            let id = (round * BUFFERS + buffer + 1) as u64;
            let mut lease = pool
                .acquire_at(buffer, support::entry(id))
                .expect("the producer's buffer is free");
            for (at, byte) in lease
                .write()
                .expect("sole owner")
                .bytes()
                .iter_mut()
                .enumerate()
            {
                *byte = pattern(round, buffer, at);
            }
            let value = Arc::new(StyxFrame::from_lease(lease, pool.buffer_size()).expect("a size"));
            host.feed_typed("frame", Arc::clone(&value))
                .expect("the frame is fed");
            host.step_until_settled(std::time::Duration::from_secs(10))
                .expect("the graph settles without a fault");
            let words = host
                .take::<Words>("words")
                .expect("the words are taken")
                .expect("the shader's output arrived")
                .to_vec();
            for (word, &got) in words.iter().take(PAYLOAD_WORDS).enumerate() {
                let at = word * 4;
                let want = u32::from_le_bytes([
                    pattern(round, buffer, at),
                    pattern(round, buffer, at + 1),
                    pattern(round, buffer, at + 2),
                    pattern(round, buffer, at + 3),
                ])
                .wrapping_add(1);
                assert_eq!(
                    got, want,
                    "bound {kept_imports}: round {round}, buffer {buffer}, word {word}"
                );
            }
            drop(value);
        }
    }
    let after = gpu.transfer_stats().expect("an open device");
    let imports = after.imports - before.imports;
    let uploads = after.uploads - before.uploads;
    println!(
        "kept_imports {kept_imports}: {} frames, imports {imports}, uploads {uploads}",
        ROUNDS * BUFFERS
    );
    Some((imports, uploads))
}

#[test]
fn a_bound_smaller_than_the_cycle_still_delivers_every_frame_its_own_bytes() {
    let Some((imports, uploads)) = cycle(2) else {
        return;
    };
    // Where the device imports dma-bufs, a bound below the cycle imports every frame again. Where
    // it doesn't, every frame is uploaded instead, and that is the same bytes.
    assert!(
        imports == (ROUNDS * BUFFERS) as u64 || uploads == (ROUNDS * BUFFERS) as u64,
        "imports {imports}, uploads {uploads}"
    );
    if imports > 0 {
        assert_eq!(
            imports,
            (ROUNDS * BUFFERS) as u64,
            "each frame is imported again"
        );
    }
}

#[test]
fn the_default_bound_imports_each_buffer_once_and_finds_it_again() {
    let Some((imports, uploads)) = cycle(styx_daedalus::GpuOptions::default().kept_imports) else {
        return;
    };
    assert!(
        imports == BUFFERS as u64 || uploads == (ROUNDS * BUFFERS) as u64,
        "imports {imports}, uploads {uploads}"
    );
}
