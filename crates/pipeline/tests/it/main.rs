//! `styx-pipeline`'s integration tests in one binary: one module per former `tests/*.rs` file, so an
//! edit relinks one test binary instead of 2 (docs/development.md#the-gate).

#[cfg(feature = "gpu")]
mod gpu_loop;
mod replay_loop;
