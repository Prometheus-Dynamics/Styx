//! `styx-gpuisp`'s integration tests in one binary: one module per former `tests/*.rs` file, so an
//! edit relinks one test binary instead of 4 (docs/development.md#the-gate).

mod alloc;
mod common;
mod convert;

#[cfg(feature = "daedalus")]
mod daedalus;
mod dmabuf;
mod equivalence;
mod fp16;
mod h264;
mod jpeg;
mod post;
mod quality;
mod shaders;
mod spirv;
mod variants;
