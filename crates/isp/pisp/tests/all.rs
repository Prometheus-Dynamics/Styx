//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

mod sim;

#[path = "colour_formats.rs"]
mod colour_formats;
#[path = "dev_golden.rs"]
mod dev_golden;
#[path = "live_order.rs"]
mod live_order;
#[path = "live_sim.rs"]
mod live_sim;
#[path = "stats_jobs_slices.rs"]
mod stats_jobs_slices;
