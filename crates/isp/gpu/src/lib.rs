//! The software ISP's pipeline (`styx-soft`) as Vulkan compute shaders.
//!
//! [`GpuIsp`] takes the same [`IspParams`](styx_soft::IspParams), raw formats, output buffers
//! and statistics as [`styx_soft::SoftIsp`] and computes the same integer arithmetic bit for
//! bit: unpacking (8/16-bit, CSI-2 RAW10/RAW12), black level, white balance and digital gain,
//! lens shading, bilinear, Malvar-He-Cutler or edge-directed demosaic, colour matrix, tone
//! table or quadratics, RGB24 / NV12 / I420 / luma at full or half size, the output stages
//! (denoise, sharpening) when the parameters ask, and the 3A statistics (zones, histogram).
//!
//! [`GpuIspStage`] is the frame interface, the GPU twin of [`styx_soft::SoftIspStage`]: a raw
//! `FrameLease` in, a picture lease out, statistics into a caller's `Statistics`. A raw frame
//! on a dma-buf is imported once per buffer and read in place.
//!
//! * One submission per frame and one wait on its fence; buffers, descriptor sets and the
//!   command buffer are made once. No per-dispatch CPU wait.
//! * The GPU lease hook ([`GpuLease`]): asked before a frame's GPU work, released after it.
//!   Unset, it costs nothing.
//! * Vulkan through `ash`, loaded at run time. The shaders are GLSL compiled to SPIR-V by
//!   `shaders/build.sh`; the SPIR-V is committed, so building needs no shader compiler.
//!
//! Everything is behind the `vulkan` feature (off by default), so a build without it, and CI
//! without Vulkan, pass. With the feature, tests skip with a printed reason when no Vulkan
//! device exists.

#![cfg(feature = "vulkan")]

mod context;
mod dmabuf;
mod error;
mod input;
mod isp;
mod kernels;
mod layout;
mod lease;
mod memory;
mod params;
mod stage;
mod stages;
mod stats;

pub use context::{
    DeviceInfo, DeviceKind, DeviceSelect, GpuContext, devices, vulkan_instance_lock,
};
pub use error::GpuError;
pub use input::{ExternalBuffer, Input};
pub use isp::{ExportedFrame, GpuIsp, ImportId};
pub use layout::{Layout, OutputKind, Plane};
pub use lease::GpuLease;
pub use stage::{GpuIspStage, StageError};
