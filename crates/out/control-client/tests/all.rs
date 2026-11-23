//! The crate's integration tests, one module per file, built as one test binary.
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "client.rs"]
mod client;
