//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[cfg(feature = "vulkan")]
mod common;

#[path = "dmabuf.rs"]
mod dmabuf;
#[path = "equals_soft.rs"]
mod equals_soft;
#[path = "lease.rs"]
mod lease;
