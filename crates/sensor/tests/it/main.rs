//! `styx-sensor`'s integration tests in one binary: one module per former `tests/*.rs` file, so an
//! edit relinks one test binary instead of 4 (docs/development.md#the-gate).
//! `no_alloc` stays a binary of its own: it installs a counting `#[global_allocator]`.

mod async_driver;
mod lens_format;
mod ov9782;
mod schema;
