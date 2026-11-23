//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "known_frames.rs"]
mod known_frames;
#[path = "service.rs"]
mod service;
#[path = "socket_access.rs"]
mod socket_access;
#[path = "stall.rs"]
mod stall;
