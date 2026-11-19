//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "dma_buf.rs"]
mod dma_buf;
#[path = "native_path.rs"]
mod native_path;
#[path = "stall.rs"]
mod stall;
