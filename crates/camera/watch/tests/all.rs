//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "replay.rs"]
mod replay;
#[path = "simulated.rs"]
mod simulated;
#[path = "vivid.rs"]
mod vivid;
