//! Why a still could not be made.

use std::fmt;

/// A still that could not be made.
#[derive(Debug)]
pub enum StillError {
    /// The request asks for nothing, or for something this crate does not make.
    Request(String),
    /// The frame's format is not one a still can be made from (not a Bayer raw format, or a
    /// picture format the software ISP does not write at full size).
    Format(String),
    /// The frame's buffer cannot be read from the CPU.
    NotReadable,
    /// The output buffer for a picture could not be made.
    Buffer(String),
    /// The software ISP refused the frame.
    Isp(styx_soft::StageError),
    /// The DNG writer refused the samples or the metadata.
    Dng(styx_dng::DngError),
}

impl fmt::Display for StillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(text) => write!(f, "still request: {text}"),
            Self::Format(text) => write!(f, "still format: {text}"),
            Self::NotReadable => write!(f, "the frame cannot be read from the CPU"),
            Self::Buffer(text) => write!(f, "still buffer: {text}"),
            Self::Isp(e) => write!(f, "software ISP: {e}"),
            Self::Dng(e) => write!(f, "DNG: {e}"),
        }
    }
}

impl std::error::Error for StillError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Isp(e) => Some(e),
            Self::Dng(e) => Some(e),
            _ => None,
        }
    }
}

impl From<styx_soft::StageError> for StillError {
    fn from(e: styx_soft::StageError) -> Self {
        Self::Isp(e)
    }
}

impl From<styx_dng::DngError> for StillError {
    fn from(e: styx_dng::DngError) -> Self {
        Self::Dng(e)
    }
}
