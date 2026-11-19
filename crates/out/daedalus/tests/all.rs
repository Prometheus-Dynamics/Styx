//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[cfg(all(feature = "daedalus", target_os = "linux"))]
mod support;

#[path = "fault_codes.rs"]
mod fault_codes;
#[path = "gpu.rs"]
mod gpu;
#[path = "graph.rs"]
mod graph;
#[path = "kept.rs"]
mod kept;
#[path = "no_heap.rs"]
mod no_heap;
#[path = "offset.rs"]
mod offset;
#[path = "process.rs"]
mod process;
#[path = "ring.rs"]
mod ring;
#[path = "status.rs"]
mod status;
#[path = "values.rs"]
mod values;
