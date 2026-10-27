/// Why a GPU kernel could not start or run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GpuError {
    /// No adapter (no GPU driver, or none matches `WGPU_BACKEND` / `WGPU_ADAPTER_NAME`).
    #[error("no GPU adapter: {0}")]
    NoAdapter(String),
    /// The adapter would not make a device.
    #[error("no GPU device: {0}")]
    NoDevice(String),
    /// The image does not fit the request (sizes, stride, data length).
    #[error("bad image: {0}")]
    BadImage(String),
    /// The device failed (lost, out of memory, a failed readback).
    #[error("GPU: {0}")]
    Device(String),
}
