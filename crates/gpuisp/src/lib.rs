//! The software ISP's pipeline (`styx-softisp`) as Vulkan compute shaders.
//!
//! [`GpuIsp`] takes the same [`IspParams`](styx_softisp::IspParams), raw formats, output
//! buffers and statistics as [`styx_softisp::SoftIsp`] and computes its integer arithmetic
//! ([`Arithmetic::Int`](styx_softisp::Arithmetic::Int)) bit for bit: unpacking (8/16-bit,
//! CSI-2 RAW10/RAW12), black level, white balance and digital gain, lens shading, bilinear or
//! Malvar-He-Cutler demosaic, colour matrix, tone table, RGB24 / NV12 / I420 / luma at full
//! or half size, and the 3A statistics (zones, histogram). A pipeline can swap one for the
//! other.
//!
//! * One submission per frame and one wait (its fence): the histogram is cleared, the raw
//!   frame copied into device memory on GPUs with their own (the copy engine; same-memory
//!   devices read it in place), the picture made (one dispatch: each workgroup runs the
//!   front end once over its tile into shared memory, then demosaics from there), the
//!   statistics gathered (one workgroup per zone), and the results copied back. Buffers,
//!   descriptor sets, pipelines and the command buffer are made once.
//! * Zero-copy: capture buffers register as dma-bufs ([`GpuIsp::import_dmabuf`],
//!   `VK_EXT_external_memory_dma_buf`) and are read in place; outputs can go to a ring of
//!   exportable buffers ([`GpuIsp::process_to_export`], [`GpuIsp::export_dmabuf`]) for
//!   other devices or processes. Without the extension frames are copied in and out.
//! * Vulkan through `ash`: `libvulkan.so.1` is loaded at run time, so nothing links against
//!   it and a binary built with this crate starts (and falls back) where there is no Vulkan.
//!   The shaders are GLSL compiled to SPIR-V by `shaders/build.sh` (glslc); the SPIR-V is
//!   committed, so building needs no shader compiler.
//!
//! Devices need Vulkan 1.2 with `storageBuffer8BitAccess` and a compute queue (RADV, ANV,
//! NVIDIA, llvmpipe, Mesa v3dv on the Raspberry Pi 5's V3D 7.1). See `README.md` for the
//! design, the measured quality and the performance.

mod context;
mod error;
mod isp;
mod kernels;
mod layout;
mod memory;
mod params;
mod stats;

pub use context::{DeviceInfo, DeviceKind, DeviceSelect, GpuContext, devices};
pub use error::GpuError;
pub use isp::{ExportedFrame, GpuIsp, ImportId, Input};
pub use layout::{Layout, OutputKind, Plane};
