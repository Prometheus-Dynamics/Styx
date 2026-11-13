//! Styx's C surface: cameras, controls and frames from C, in one of two ways.
//!
//! - **In process** ([`Source::local`]): the library lists and opens cameras through `styx-select`,
//!   on the Lemnos daemon at a socket. The camera's capture runs on its own thread.
//! - **Over the socket** ([`Source::service`]): the library is a client of `styx-service`. Its
//!   control socket lists, reads and sets controls (the line protocol); each camera's frame socket
//!   delivers frames through `styx-socket`, so the planes are read where the producer wrote them.
//!
//! Both paths give the same handles and the same calls, declared in `include/styx.h` (version
//! 1.0). Handles are opaque; each call returns a status; frames are borrowed until released; no
//! panic crosses the boundary. The functions in [`surface`] are the exports; the types here
//! ([`Source`], [`Camera`], [`Frame`]) are what their handles point to, for Rust callers of the
//! rlib (the tests use them).
//!
//! Linux only: the socket path's ring is a Linux layout (`styx-socket`).
//!
//! # Not wired here
//!
//! - In-process libcamera and V4L2 cameras: the listing sees them and the chooser picks them, but
//!   the in-process path opens native cameras only; the service serves the others.
//! - Reconnection: a frame socket the producer cuts off, or a pool it replaces, ends the camera
//!   with `E_STOPPED`; the caller opens it again.
//! - Fences, and a dma-buf frame's CPU reads: the planes of such a frame are mapped for the CPU
//!   only where the memory is; the descriptor is for the device.

#![cfg(target_os = "linux")]

mod camera;
mod counters;
mod frame;
mod source;
mod status;
mod surface;
mod wire;

pub use camera::{Camera, OpenOptions};
pub use counters::CounterSnapshot;
pub use frame::{
    DmaView, Frame, FrameInfo, MEMORY_DMABUF, MEMORY_GPU, MEMORY_HOST, MEMORY_MEMFD, PlaneOut,
};
pub use source::Source;
pub use status::{
    E_BUSY, E_INVALID, E_IO, E_NOT_FOUND, E_PANIC, E_PROTOCOL, E_RANGE, E_STOPPED, E_TIMEOUT,
    E_UNSUPPORTED, OK,
};
pub use surface::*;
