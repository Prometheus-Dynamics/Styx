//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "close_on_drop.rs"]
mod close_on_drop;
#[path = "connect.rs"]
mod connect;
#[path = "discovery.rs"]
mod discovery;
#[path = "hold.rs"]
mod hold;
#[path = "sim_chain.rs"]
mod sim_chain;
#[path = "stride.rs"]
mod stride;
#[path = "zero_copy.rs"]
mod zero_copy;
