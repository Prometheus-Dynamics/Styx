//! `styx-tune`'s integration tests in one binary: one module per former `tests/*.rs` file, so an
//! edit relinks one test binary instead of 2 (docs/development.md#the-gate).

mod real_ov9782;
mod synthetic;
