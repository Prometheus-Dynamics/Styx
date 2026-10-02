//! Errors.

/// What went wrong.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The algorithms refused a configuration or tuning.
    #[error("algorithms: {0}")]
    Algo(#[from] styx_algo::AlgoError),
    /// The software ISP refused a frame or its parameters.
    #[error("software ISP: {0}")]
    SoftIsp(#[from] styx_softisp::IspError),
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
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The camera or the ISP device failed.
    #[error("device: {0}")]
    Device(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, PipelineError>;
