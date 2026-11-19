//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

mod common;

#[path = "attribution.rs"]
mod attribution;
#[path = "clamp.rs"]
mod clamp;
#[path = "frame_rate.rs"]
mod frame_rate;
#[path = "gain_law.rs"]
mod gain_law;
#[path = "landing.rs"]
mod landing;
#[path = "limits.rs"]
mod limits;
#[path = "restart.rs"]
mod restart;
#[path = "sim_backend.rs"]
mod sim_backend;
