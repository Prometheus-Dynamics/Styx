//! Errors.

use alloc::string::String;

/// What went wrong.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The algorithms refused a configuration or tuning.
    #[error("algorithms: {0}")]
    Algo(#[from] styx_algo::AlgoError),
    /// The software ISP refused a frame or its parameters.
    #[error("software ISP: {0}")]
    SoftIsp(#[from] styx_softisp::IspError),
    /// The GPU ISP could not start or process a frame.
    #[cfg(feature = "gpu")]
    #[error("GPU ISP: {0}")]
    GpuIsp(#[from] styx_gpuisp::GpuError),
    /// The sensor description does not have what is needed.
    #[error("sensor: {0}")]
    Sensor(String),
    /// A configuration is not usable.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// A recording could not be read or written.
    #[error("recording: {0}")]
    Recording(String),
    /// Reading or writing a file.
    #[cfg(feature = "std")]
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The camera or the ISP device failed.
    #[error("device: {0}")]
    Device(String),
    /// Every buffer of a back end output is held by consumers: the frame was dropped (its raw
    /// buffer went back to the camera). The next frame is processed once one is released.
    #[error("frame dropped: every buffer of output {0} is held")]
    OutputsHeld(usize),
}

/// Result alias.
pub type Result<T> = core::result::Result<T, PipelineError>;
