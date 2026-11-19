//! The crate's integration tests, one module per file, built as one test binary (a
//! binary per file links the crate and its dependencies once per file).
//! scripts/test-layout.py fails the gate when a tests/*.rs file is in no binary.

#[path = "absent.rs"]
mod absent;
#[path = "drm_prime.rs"]
mod drm_prime;
#[path = "mjpeg.rs"]
mod mjpeg;
#[path = "versions.rs"]
mod versions;
