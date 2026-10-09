//! `styx-gpuisp`'s integration tests in one binary: one module per former `tests/*.rs` file, so an
//! edit relinks one test binary instead of 4 (docs/development.md#the-gate).

mod common;

mod dmabuf;
mod equivalence;
mod quality;
mod shaders;
