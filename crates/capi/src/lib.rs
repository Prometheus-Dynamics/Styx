//! Styx's C surface: cameras, streams, frames and controls behind a small, versioned set of
//! `extern "C"` functions ([`surface`], declared in `include/styx.h`), and [`Camera`] /
//! [`Stream`] / [`Frame`], a Rust wrapper that calls through those same functions.
//!
//! The core is Styx's planner and capture, the surface is this crate's exports (`staticlib` and
//! `cdylib`), the wrapper is linked statically. Status codes, handles, buffer descriptions, the
//! version handshake and the panic guard are `lemnos-abi`'s, so a program using Lemnos's surface
//! too sees each type once.
//!
//! Granularity: one call per frame (next, describe, release), never per pixel: a call across a
//! shared library costs the frame, not the call. Static linking (fat LTO) stays the default;
//! the shared form is for consumers outside the image. A process holds one or the other: with
//! `shared` this crate exports nothing.
//!
//! The unsafe code of the C boundary lives here, one operation per block, each with a
//! `// SAFETY:` comment. Everything public in Rust is safe to call.

mod client;
mod ffi;
pub mod surface;

pub use client::{Camera, ControlValue, Error, Frame, Stream};
pub use surface::{FrameInfo, StreamRequest, VERSION};

#[cfg(test)]
mod tests;
