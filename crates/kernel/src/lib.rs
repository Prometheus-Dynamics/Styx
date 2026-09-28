//! Safe Rust interfaces to the Linux kernel's camera devices, with no C libraries: V4L2 video
//! nodes, the media controller, V4L2 subdevices, V4L2 events, dma-heaps, and the buses a
//! userspace sensor driver needs (I²C, GPIO, the Styx sensor bridge).
//!
//! See `docs/native-stack/README.md`.

#![cfg(target_os = "linux")]

pub mod bus;
pub mod dma_heap;
pub mod event;
pub mod media;
pub mod subdev;
pub mod v4l2;
