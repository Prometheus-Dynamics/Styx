//! Errors of the loop: the algorithms' own, and a sensor request a control would refuse.

use core::fmt;

use styx_aaa::AlgoError;
use styx_controls::{Control, ValueError};

/// What the loop can fail with.
#[derive(Debug)]
#[non_exhaustive]
pub enum ControllerError {
    /// Tuning, configuration or the algorithms themselves (see [`AlgoError`]).
    Algo(AlgoError),
    /// A value the algorithms asked for is outside the control's range, so no backend could
    /// post it (for example an exposure longer than the control's maximum).
    Value {
        /// The control the value was for.
        control: Control,
        /// Why it was refused.
        error: ValueError,
    },
}

/// A result of the loop.
pub type Result<T> = core::result::Result<T, ControllerError>;

impl From<AlgoError> for ControllerError {
    fn from(e: AlgoError) -> Self {
        Self::Algo(e)
    }
}

impl fmt::Display for ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Algo(e) => write!(f, "{e}"),
            Self::Value { control, error } => write!(f, "{}: {error}", control.name()),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ControllerError {}
