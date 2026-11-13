//! A Styx frame's dma-buf imported into a GPU device in place, and read by a shader.
//!
//! The frame is a buffer of the system dma-heap, the way a camera service's pool hands them out.
//! The host feeds it to a graph whose one node is a shader; the planner takes the device's import
//! of the `styx:frame` value over any copy (a `styx:frame` has no byte upload on this device), and
//! the shader reads the frame's bytes where they were written. The test skips, with the reason
//! printed, when the dma-heap can't be opened or no usable device opens. `STYX_DAEDALUS_ADAPTER`
//! names the adapter (a substring of its name) to run on; by default the backend's best one.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

mod support;

use std::sync::Arc;

use daedalus::prelude::*;
use daedalus_cpu::CpuBackend;
use daedalus_dmabuf::DmaBufMemory;
use daedalus_wgpu::{HostBytes, Options, WgpuBackend};
use styx_daedalus::{StyxFrame, dmabuf_memory};
use styx_frame::copy_counters;

/// What the shader gives back: the words of the device's output, as bytes.
#[derive(Clone, Debug, Type)]
#[type_key("gpu:words")]
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

/// Words of the frame's payload (4096 bytes). The shader's output is as long as its input, the
/// whole imported allocation (payload and GPU slack), so only the payload's words are checked.
const PAYLOAD_WORDS: usize = 4096 / 4;
const ALLOCATION_WORDS: usize = (4096 + 256) / 4;

/// On the GPU: each word of the frame, plus one.
#[node(
    id = "styx_test.gpu_plus_one",
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
       "port_types": {"frame": "styx:frame", "words": "gpu:words"}},
      {"id": "styx_test.gpu_plus_one"}
    ],
    "edges": [
      {"from": {"node": 0, "port": "frame"}, "to": {"node": 1, "port": "src"}},
      {"from": {"node": 1, "port": "dst"}, "to": {"node": 0, "port": "words"}}
    ]
  }
}"#;

/// The byte the test writes at `at` of the frame's payload.
fn pattern(at: usize) -> u8 {
    (at as u8).wrapping_mul(7).wrapping_add(1)
}

/// The device, opened with the import of `StyxFrame` and the readback of `Words`. Err with the
/// reason when no usable device opens here.
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

#[test]
fn a_dma_heap_frame_is_imported_into_the_gpu_and_the_shader_reads_its_bytes() {
    let Some(pool) = support::dma_pool(2) else {
        return;
    };
    let gpu = match open_gpu() {
        Ok(gpu) => gpu,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    let caps = gpu.caps().expect("a device after start");
    println!("gpu: {:?} dma_heap={}", caps.adapter, caps.dma_heap);

    let mut lease = pool.acquire(support::entry(9)).expect("a free buffer");
    let index = lease.buffer_index();
    for (at, byte) in lease
        .write()
        .expect("sole owner")
        .bytes()
        .iter_mut()
        .enumerate()
    {
        *byte = pattern(at);
    }
    let frame = StyxFrame::from_lease(lease, pool.buffer_size()).expect("an allocation");
    let holders_made = frame.holders();
    let value = Arc::new(frame);
    println!("holders after the lease became a value: {holders_made}");

    let mut registry = Registry::new();
    registry
        .register_all(&[&PLUS_ONE_NODE])
        .expect("the shader node registers");
    let graph = GraphDoc::parse(GRAPH).expect("the graph parses").graph;
    let dma: DmaBufMemory = dmabuf_memory();
    let backends: [&dyn daedalus_backend::Backend; 3] = [&gpu, &CpuBackend, &dma];
    let planned = plan(&graph, &registry, &backends, &Config::default()).expect("the graph plans");
    let moves: Vec<&str> = planned
        .stitched
        .transfers
        .iter()
        .map(|used| used.id.as_str())
        .collect();
    println!("moves: {moves:?}");
    assert!(
        moves
            .iter()
            .any(|id| id.contains("import") && id.contains("styx:frame")),
        "the device imports the frame: {moves:?}"
    );

    let mut loaded = LoadedHost::load(planned, LoadOptions::default()).expect("the graph loads");
    let host = loaded.host();
    host.feed_typed("frame", Arc::clone(&value))
        .expect("the frame is fed");
    println!("holders with the host's reference out: {}", value.holders());
    assert_eq!(value.holders(), holders_made, "a reference is not a lease");
    host.step_until_settled(std::time::Duration::from_secs(10))
        .expect("the graph settles");
    let words = host
        .take::<Words>("words")
        .expect("the words are taken")
        .expect("the shader's output arrived");
    let got = words.to_vec();
    assert_eq!(
        got.len(),
        ALLOCATION_WORDS,
        "the output is the imported allocation"
    );
    for (word, &value_word) in got.iter().take(PAYLOAD_WORDS).enumerate() {
        let at = word * 4;
        let want = u32::from_le_bytes([
            pattern(at),
            pattern(at + 1),
            pattern(at + 2),
            pattern(at + 3),
        ])
        .wrapping_add(1);
        assert_eq!(
            value_word, want,
            "word {word}: the device read the frame's bytes"
        );
    }
    println!("holders after the shader read it: {}", value.holders());
    assert_eq!(value.holders(), holders_made);

    drop(loaded);
    assert!(
        !pool.is_free(index),
        "the host's reference still holds the buffer"
    );
    drop(value);
    assert!(
        pool.is_free(index),
        "the last reference returned the buffer"
    );
    let copies: Vec<usize> = copy_counters()
        .iter()
        .map(|counter| counter.copies())
        .collect();
    assert!(copies.iter().all(|&made| made == 0), "copies: {copies:?}");
}
