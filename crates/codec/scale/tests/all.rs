//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

mod sim;
mod support;

#[path = "lemnos_sim.rs"]
mod lemnos_sim;
#[path = "software.rs"]
mod software;
