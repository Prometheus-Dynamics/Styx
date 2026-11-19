//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "controls.rs"]
mod controls;
#[path = "cycles.rs"]
mod cycles;
#[path = "ended.rs"]
mod ended;
#[path = "ends_sim.rs"]
mod ends_sim;
#[path = "recovery.rs"]
mod recovery;
#[path = "session_sim.rs"]
mod session_sim;
