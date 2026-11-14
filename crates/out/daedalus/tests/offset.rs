//! Frames cut from one descriptor at different offsets, each sent through a GPU graph.
//!
//! A dma-buf frame at an offset names its whole dma-buf and says where it starts
//! (`DmaBufBacked::offset`), so a device that can bind there imports it in place; a device binds
//! only at a multiple of its own alignment, and any other offset is copied. Each case runs ten
//! frames, one trip through a freshly loaded graph each, and every trip is one frame with nothing
//! else moving, so the device's counter deltas for it are that frame's:
//!
//! - the device's bind alignment is not reported by the rev (the import error carries it, but the
//!   copy path discards that error), so it is inferred: one trip at each power of two offset
//!   imports or copies; the smallest that imports is the alignment, and it is printed as inferred;
//! - the page-multiple frames must arrive with their own bytes, and where the offset is a multiple
//!   of that alignment each frame is imported with nothing uploaded for it (`imports` +1,
//!   `uploads` +0, and the plan has no upload move);
//! - the offset of 100 is misaligned or not by that alignment, and the test expects what that
//!   alignment says: copied (`imports` +0, `uploads` +1) or imported (`imports` +1); each frame
//!   must arrive with its own bytes either way.
//!
//! The memfd case has no dma-buf, so its frames reach the device through their bytes, from the
//! frame's own start: copied, never imported. Each frame is written with its own pattern and the
//! shader must read each frame's pattern, so a read from the descriptor's start would fail. The
//! dma-heap cases skip, with the reason printed, when the heap or a GPU device can't open.
#![cfg(all(feature = "daedalus", target_os = "linux"))]

use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, PoisonError};

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
/// The frames of a case, one per buffer of its pool.
const FRAMES: usize = 3;
/// Trips through the graph per case: every case runs this many frames.
const TRIPS: usize = 10;
/// The frames of the aligned case start on a page and a page apart from one another.
const ALIGNED: [usize; FRAMES] = [4096, 4096 + 8192, 4096 + 2 * 8192];
/// The frames of the misaligned case: the first starts 100 bytes in, so the mapping's lead is used.
const MISALIGNED: [usize; FRAMES] = [100, 100 + 4352, 100 + 2 * 4352];
const MISALIGNED_OFFSET: usize = 100;
/// The offsets a probe tries, one trip each: the powers of two a device's alignment can be.
const PROBES: [usize; 13] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];

/// The GPU tests of this binary share one process-wide device, so their counter deltas overlap when
/// they run in parallel (a trip of one counts another's imports). Each test holds this for its run.
static GPU_TURN: Mutex<()> = Mutex::new(());

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

/// One frame per offset, each a window of `kind` memory at its offset in one descriptor, in one
/// pool. Every frame is written with its own pattern and turned into a `styx:frame` value.
fn frames_from_one_descriptor(
    fd: &OwnedFd,
    kind: MemoryKind,
    offsets: &[usize],
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
/// plan made, and what the device counted for this trip. The graph is loaded for this one frame,
/// so nothing else is in the trip.
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

/// One trip: the frame, where it was cut, what the plan moved, and the device's counts for it.
struct Trip {
    offset: usize,
    moves: Vec<String>,
    imports: u64,
    uploads: u64,
}

/// `TRIPS` trips through the graph, cycling over `values`. Each trip's frame must read its own
/// bytes. Printed per trip, so the counters are visible in the test output.
fn trips(gpu: &WgpuBackend, values: &[Made], label: &str) -> Vec<Trip> {
    let mut out = Vec::new();
    for trip in 0..TRIPS {
        let (id, offset, value) = &values[trip % values.len()];
        let (words, moves, counted) = through_graph(gpu, Arc::clone(value));
        check(&words, *id, label);
        println!(
            "{label}: trip {trip:>2} frame {id} at offset {offset}: imports +{}, uploads +{}, moves {moves:?}",
            counted.imports, counted.uploads
        );
        out.push(Trip {
            offset: *offset,
            moves,
            imports: counted.imports,
            uploads: counted.uploads,
        });
    }
    out
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

/// The device's bind alignment, inferred: one trip at each power-of-two offset of a dma-buf, which
/// imports (the offset is a multiple of the alignment) or copies. The smallest offset that imports
/// is the alignment. Returns it with each probe's answer. `None` when nothing probed imports.
///
/// The rev reports no alignment: `import_dma_buf_at` refuses an offset with `Error::Offset {
/// alignment }`, but the wgpu import step turns that refusal into a copy and drops the error.
fn probe_alignment(gpu: &WgpuBackend, fd: &OwnedFd) -> (Option<usize>, Vec<(usize, bool)>) {
    let mut answers = Vec::new();
    for offset in PROBES {
        let (_, values) = frames_from_one_descriptor(fd, MemoryKind::Dmabuf, &[offset]);
        let (_, _, value) = &values[0];
        let (_, _, counted) = through_graph(gpu, Arc::clone(value));
        answers.push((offset, counted.imports == 1));
    }
    let alignment = answers
        .iter()
        .find(|(_, aligned)| *aligned)
        .map(|(o, _)| *o);
    (alignment, answers)
}

/// Whether a frame at `offset` is imported: the device imports it when `offset` is a multiple of its
/// alignment, and copies it otherwise. An alignment of `None` (nothing imports) copies every frame.
fn imported_at(alignment: Option<usize>, offset: usize) -> bool {
    alignment.is_some_and(|a| offset % a == 0)
}

fn print_probe(label: &str, alignment: Option<usize>, answers: &[(usize, bool)]) {
    let shown: Vec<String> = answers
        .iter()
        .map(|(o, aligned)| format!("{o}:{}", if *aligned { "imports" } else { "copies" }))
        .collect();
    println!("{label}: probes {}", shown.join(" "));
    match alignment {
        Some(a) => println!(
            "{label}: device bind alignment, inferred (the rev does not report it): {a} bytes"
        ),
        None => println!("{label}: no probed offset imports: the device copies every dma-buf"),
    }
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
    let _turn = GPU_TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let len = MISALIGNED[FRAMES - 1] + SIZE.allocation().unwrap();
    let fd = dma_buf::sealed_memfd(c"styx-daedalus-offset", len as u64).expect("a memfd");
    let (_pool, values) = frames_from_one_descriptor(&fd, MemoryKind::Memfd, &MISALIGNED);
    for trip in trips(&gpu, &values, "memfd") {
        // A memfd names no dma-buf: its bytes are uploaded, and nothing is imported.
        assert_eq!(trip.imports, 0, "memfd at {}: never imported", trip.offset);
        assert_eq!(
            trip.uploads, 1,
            "memfd at {}: its bytes are uploaded",
            trip.offset
        );
    }
}

#[test]
fn dma_heap_frames_at_page_offsets_each_read_their_own_bytes_and_are_imported_in_place() {
    let gpu = match open_gpu() {
        Ok(gpu) => gpu,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    let _turn = GPU_TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let len = 4096 + ALIGNED[FRAMES - 1] + SIZE.allocation().unwrap();
    let Some(fd) = heap_buffer(len) else {
        return;
    };
    println!(
        "dma-heap page: before any trip, {}",
        gpu.dmabuf_status(false)
    );
    let (alignment, answers) = probe_alignment(&gpu, &fd);
    print_probe("dma-heap page", alignment, &answers);
    println!(
        "dma-heap page: after the probes, {}",
        gpu.dmabuf_status(false)
    );
    let (_pool, values) = frames_from_one_descriptor(&fd, MemoryKind::Dmabuf, &ALIGNED);
    for (id, offset, value) in &values {
        assert_eq!(
            value.offset(),
            *offset as u64,
            "frame {id}: its offset is its own"
        );
    }
    let expect = |offset: usize| imported_at(alignment, offset);
    for trip in trips(&gpu, &values, "dma-heap page") {
        if expect(trip.offset) {
            // Imported in place: the import step uploaded nothing for this frame. The plan has no
            // upload move, so no other step could have made the upload either.
            assert_eq!(trip.imports, 1, "offset {}: imported in place", trip.offset);
            assert_eq!(
                trip.uploads, 0,
                "offset {}: nothing uploaded for it",
                trip.offset
            );
            assert!(
                trip.moves.iter().any(|m| m.starts_with("wgpu.import:"))
                    && !trip.moves.iter().any(|m| m.contains("upload")),
                "offset {}: the plan imports and does not upload: {:?}",
                trip.offset,
                trip.moves
            );
        } else {
            assert_eq!(
                trip.imports, 0,
                "offset {}: copied, not imported",
                trip.offset
            );
            assert_eq!(trip.uploads, 1, "offset {}: copied", trip.offset);
        }
    }
}

#[test]
fn dma_heap_frames_at_a_misaligned_offset_read_their_own_bytes_as_the_device_alignment_says() {
    let gpu = match open_gpu() {
        Ok(gpu) => gpu,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    let _turn = GPU_TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let len = 4096 + MISALIGNED[FRAMES - 1] + SIZE.allocation().unwrap();
    let Some(fd) = heap_buffer(len) else {
        return;
    };
    println!(
        "dma-heap offset {MISALIGNED_OFFSET}: before any trip, {}",
        gpu.dmabuf_status(false)
    );
    let (alignment, answers) = probe_alignment(&gpu, &fd);
    print_probe("dma-heap offset 100", alignment, &answers);
    let (_pool, values) = frames_from_one_descriptor(&fd, MemoryKind::Dmabuf, &MISALIGNED);
    assert_eq!(values[0].1, MISALIGNED_OFFSET);
    let expect = imported_at(alignment, MISALIGNED_OFFSET);
    println!(
        "dma-heap offset {MISALIGNED_OFFSET}: {} alignment {} so {}",
        if expect {
            "a multiple of"
        } else {
            "not a multiple of"
        },
        alignment.map_or_else(|| "unknown".to_string(), |a| a.to_string()),
        if expect {
            "imported in place"
        } else {
            "copied"
        }
    );
    for trip in trips(&gpu, &values, "dma-heap offset 100") {
        if expect {
            assert_eq!(trip.imports, 1, "offset 100 is aligned: imported");
            assert_eq!(trip.uploads, 0, "offset 100 is aligned: nothing uploaded");
        } else {
            // The device refuses the offset, the frame is copied, and the import count stays 0 for
            // every trip, the trial included (a refusal of the value does not decide the device).
            assert_eq!(trip.imports, 0, "offset 100 is misaligned: no import");
            assert_eq!(trip.uploads, 1, "offset 100 is misaligned: copied");
        }
    }
    println!(
        "dma-heap offset {MISALIGNED_OFFSET}: after the trips, {}",
        gpu.dmabuf_status(false)
    );
}
