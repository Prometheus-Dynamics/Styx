use ash::vk;
use styx_softisp::IspError;

/// Why the GPU ISP could not start or process a frame.
#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    /// No Vulkan loader (`libvulkan.so.1`), or it could not make an instance.
    #[error("Vulkan is not available: {0}")]
    NoVulkan(String),
    /// Vulkan works but no device suits (Vulkan 1.2, 8-bit storage buffers, a compute queue),
    /// or none matches the selection.
    #[error("no suitable Vulkan device: {0}")]
    NoDevice(String),
    /// A Vulkan call failed.
    #[error("{call}: {result}")]
    Vulkan {
        call: &'static str,
        result: vk::Result,
    },
    /// The device lacks something this request needs (e.g. dma-buf import).
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The parameters, frame or output buffers, as `styx-softisp` would report them.
    #[error(transparent)]
    Isp(#[from] IspError),
}

pub(crate) trait VkContext<T> {
    fn ctx(self, call: &'static str) -> Result<T, GpuError>;
}

impl<T> VkContext<T> for Result<T, vk::Result> {
    fn ctx(self, call: &'static str) -> Result<T, GpuError> {
        self.map_err(|result| GpuError::Vulkan { call, result })
    }
}
