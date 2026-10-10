//! Safe Rust interfaces to the Linux kernel's camera devices, with no C libraries: V4L2 video
//! nodes, the media controller, V4L2 subdevices, V4L2 events, dma-heaps, the Styx sensor bridge
//! and usbfs. Generic buses (I²C, GPIO), embedded-hal over them and kernel uevents are Lemnos's
//! (`lemnos-linux`).
//!
//! Every device type owns its file descriptor and implements [`std::os::fd::AsFd`], so an async
//! layer can register it with any reactor. Devices are opened non-blocking: dequeue calls return
//! `Ok(None)` when nothing is ready instead of blocking.
//!
//! The kernel uAPI structures are defined here by hand (`#[repr(C)]`) with compile-time size
//! and layout checks for 64-bit targets (x86_64 and aarch64).
//!
//! See `docs/native-stack/README.md`.

#![cfg(target_os = "linux")]

pub mod bus;
mod clock;
pub mod dma_heap;
mod error;
pub mod event;
mod flags;
mod fourcc;
mod geometry;
mod ioctl;
mod mapping;
pub mod media;
pub mod subdev;
pub mod usbfs;
pub mod v4l2;

pub use clock::monotonic_now;
pub use error::{Error, Result};
pub use fourcc::FourCc;
pub use geometry::{Fraction, Rect};
pub use ioctl::{Ready, Wait, poll, poll_into};
pub use mapping::Mapping;
