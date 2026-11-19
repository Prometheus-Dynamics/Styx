//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

mod common;

#[path = "compare.rs"]
mod compare;
#[path = "interpolate.rs"]
mod interpolate;
#[path = "serialise.rs"]
mod serialise;
