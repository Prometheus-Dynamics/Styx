//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod common;

#[path = "consumer.rs"]
mod consumer;
#[path = "ended.rs"]
mod ended;
#[path = "reconnect.rs"]
mod reconnect;
#[path = "stalled.rs"]
mod stalled;
