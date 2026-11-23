//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

mod support;

#[path = "channels.rs"]
mod channels;
#[path = "fuzz.rs"]
mod fuzz;
#[path = "mcap_export.rs"]
mod mcap_export;
#[path = "mcap_import.rs"]
mod mcap_import;
#[path = "record_replay.rs"]
mod record_replay;
#[path = "refusals.rs"]
mod refusals;
