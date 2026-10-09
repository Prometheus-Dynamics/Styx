//! `styx-softisp`'s integration tests in one binary: one module per former `tests/*.rs` file, so an
//! edit relinks one test binary instead of 5 (docs/development.md#the-gate).

mod common;

mod golden;
mod pipeline;
mod quality;
mod regions;
mod stats;
