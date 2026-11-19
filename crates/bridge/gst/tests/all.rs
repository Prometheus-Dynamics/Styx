//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "element.rs"]
mod element;
#[path = "ended.rs"]
mod ended;
#[path = "mjpg.rs"]
mod mjpg;
#[path = "reconnect.rs"]
mod reconnect;
#[path = "source.rs"]
mod source;
