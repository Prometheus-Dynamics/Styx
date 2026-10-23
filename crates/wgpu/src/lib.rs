//! Optional GPU frame preparation for Styx on wgpu (Vulkan, Metal, DX12, GLES).
//!
//! The CPU and the ISP stay Styx's default path; this crate is for applications that want work
//! off the CPU and have a GPU to spare. Its kernels compute exactly what the CPU path computes,
//! so swapping one for the other changes no pixel.
//!
//! - [`Gpu`]: a device and queue, made here ([`Gpu::new`]) or shared with the application's
//!   own wgpu (Daedalus, Bevy: [`Gpu::from_device`]).
//! - [`BoxPyramid`]: pyramid levels of an 8-bit luma image, each the level above halved by
//!   the rounded 2x2 box filter `(a + b + c + d + 2) >> 2` (styx-core's
//!   `box_downscale_luma`, odd sizes rounded down), all levels in one submission.

mod error;
mod pyramid;

pub use error::GpuError;
pub use pyramid::{BoxPyramid, Level, Luma};

/// A wgpu device and queue for Styx's kernels.
#[derive(Clone, Debug)]
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    name: String,
}

impl Gpu {
    /// The system's preferred adapter (high performance first), headless, with its limits.
    /// `WGPU_BACKEND` and `WGPU_ADAPTER_NAME` select another, as in wgpu.
    pub fn new() -> Result<Self, GpuError> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(wgpu::util::initialize_adapter_from_env_or_default(
            &instance, None,
        ))
        .map_err(|e| GpuError::NoAdapter(e.to_string()))?;
        let name = adapter.get_info().name;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("styx-wgpu"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| GpuError::NoDevice(e.to_string()))?;
        Ok(Self {
            device,
            queue,
            name,
        })
    }

    /// The application's device and queue, shared.
    pub fn from_device(device: wgpu::Device, queue: wgpu::Queue, name: impl Into<String>) -> Self {
        Self {
            device,
            queue,
            name: name.into(),
        }
    }

    /// The adapter's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
}

#[cfg(test)]
mod tests;
