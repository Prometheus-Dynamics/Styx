//! Why a control request was refused.

use std::fmt;

use lemnos_core::Status;
use styx_controls::{Control, ValueError};

/// A value outside this sensor's own limits for the mode it runs. The bounds are in the control's
/// unit: microseconds for exposure, thousandths for gain, millihertz for frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    /// The control asked for.
    pub control: Control,
    /// The value asked for, in the control's unit.
    pub requested: u32,
    /// Smallest value the sensor takes, in the control's unit.
    pub min: u32,
    /// Largest value the sensor takes, in the control's unit.
    pub max: u32,
    /// The frame length (lines) the bounds are at, for an exposure bounded by its frame.
    pub frame_lines: Option<u32>,
}

impl Limit {
    fn unit(&self) -> &'static str {
        match self.control {
            Control::Exposure => "us",
            Control::Gain => "milli",
            Control::FrameRate => "mHz",
            _ => "",
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = self.unit();
        write!(
            f,
            "{} {} {unit} is outside the sensor's {}..={} {unit}",
            self.control.name(),
            self.requested,
            self.min,
            self.max
        )?;
        if let Some(lines) = self.frame_lines {
            write!(f, " at a frame length of {lines} lines")?;
        }
        Ok(())
    }
}

/// Errors from a native control call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The value is not one the control takes (the generic range from `styx-controls`).
    Value(ValueError),
    /// The value is outside this sensor's own limits for the mode. Nothing was posted.
    Limit(Limit),
    /// Automatic exposure, gain or frame rate was asked for. The sensor path posts values; the
    /// automatic choice is the 3A loop's, which sends values.
    Auto(Control),
    /// The control is not one this path posts. White balance and focus are the ISP's and the lens',
    /// flips and crop have no call on this path.
    Unsupported(Control),
    /// The same control appears twice in one request.
    Duplicate(Control),
    /// The mode cannot be used: no line time, or a gain range with no unity code.
    Mode(&'static str),
    /// No frame has started, so there is no frame to land a value on. Call `frame_start` first.
    NotStreaming,
    /// The sensor refused a call. Nothing was recorded for it.
    Sensor(Status),
    /// The gain law is unknown, so no gain code can be made. The contract at the lock has no
    /// gain law; Lemnos J4 adds it.
    GainLaw,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Value(e) => write!(f, "control value: {e}"),
            Error::Limit(limit) => write!(f, "{limit}"),
            Error::Auto(c) => write!(f, "{} has no sensor value for auto", c.name()),
            Error::Unsupported(c) => write!(f, "{} is not posted to the sensor here", c.name()),
            Error::Duplicate(c) => write!(f, "{} is in the request twice", c.name()),
            Error::Mode(why) => write!(f, "sensor mode cannot take controls: {why}"),
            Error::NotStreaming => write!(f, "no frame has started"),
            Error::Sensor(s) => write!(f, "sensor refused the call: {s:?}"),
            Error::GainLaw => write!(
                f,
                "gain law unknown: Lemnos J4 adds it to the contract; set GainLaw::Linear only for a sensor known to be linear"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<ValueError> for Error {
    fn from(e: ValueError) -> Self {
        Error::Value(e)
    }
}
