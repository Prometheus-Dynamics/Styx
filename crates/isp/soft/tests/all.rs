//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

mod common;

#[path = "golden.rs"]
mod golden;
#[path = "paths.rs"]
mod paths;
#[path = "stats_scale.rs"]
mod stats_scale;
#[path = "switch.rs"]
mod switch;
