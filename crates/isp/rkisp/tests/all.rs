//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "dev_goldens.rs"]
mod dev_goldens;
#[path = "job_sim.rs"]
mod job_sim;
