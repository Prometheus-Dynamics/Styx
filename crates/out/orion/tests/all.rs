//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[cfg(all(target_os = "linux", feature = "orion"))]
mod common;

#[path = "adopt.rs"]
mod adopt;
#[path = "advert.rs"]
mod advert;
#[path = "orion.rs"]
mod orion;
