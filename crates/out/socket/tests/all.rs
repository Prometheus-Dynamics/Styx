//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "attach.rs"]
mod attach;
#[path = "dmabuf.rs"]
mod dmabuf;
#[path = "ended.rs"]
mod ended;
#[path = "keep.rs"]
mod keep;
#[path = "pool.rs"]
mod pool;
#[path = "reader_sets.rs"]
mod reader_sets;
#[path = "reserve.rs"]
mod reserve;
#[path = "ring.rs"]
mod ring;
#[path = "stall.rs"]
mod stall;
#[path = "version.rs"]
mod version;
