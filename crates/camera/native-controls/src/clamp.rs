//! A value the sensor cannot take as asked, moved to the nearest value it can take, and why.
//!
//! The rule is the project's: a request that cannot be met exactly is answered with the nearest
//! value the sensor can do, and the answer says why. Exposure is bounded by the frame length in
//! force on its frame (the sensor's margin and minimum); gain by the sensor's codes. Frame rate has
//! priority: a frame length is never lengthened to fit an exposure, so an exposure is what moves.

use styx_controls::Control;

/// Which end of the sensor's range a value was moved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClampReason {
    /// The value is above what the sensor takes here (for exposure: what the frame length allows).
    Ceiling,
    /// The value is below what the sensor takes (for exposure: the sensor's minimum).
    Floor,
}

impl ClampReason {
    /// The reason as one word, for the service's status.
    pub const fn name(self) -> &'static str {
        match self {
            ClampReason::Ceiling => "ceiling",
            ClampReason::Floor => "floor",
        }
    }
}

/// A value moved to the nearest one the sensor takes, with the reason. Values are in the control's
/// unit: microseconds for exposure, thousandths for gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clamp {
    /// The control that was clamped.
    pub control: Control,
    /// The value asked for.
    pub requested: u32,
    /// The value applied: the nearest one the sensor takes.
    pub applied: u32,
    /// The frame length (lines) the bound is at, for an exposure.
    pub frame_lines: Option<u32>,
    pub reason: ClampReason,
    /// The reason in a sentence, with the numbers (for example `exposure asked 10000 us, the frame
    /// is 915 lines, exposure can be at most 8099 us: applied 8099 us`).
    pub text: String,
}

impl Clamp {
    pub(crate) fn new(
        control: Control,
        requested: u32,
        applied: u32,
        frame_lines: Option<u32>,
        reason: ClampReason,
    ) -> Self {
        let (what, unit) = match control {
            Control::Exposure => ("exposure", "us"),
            _ => ("gain", "milli"),
        };
        let bound = match reason {
            ClampReason::Ceiling => "at most",
            ClampReason::Floor => "at least",
        };
        let mut text = format!("{what} asked {requested} {unit}, ");
        if let Some(lines) = frame_lines {
            text.push_str(&format!("the frame is {lines} lines, "));
        }
        text.push_str(&format!(
            "{what} can be {bound} {applied} {unit}: applied {applied} {unit}"
        ));
        Self {
            control,
            requested,
            applied,
            frame_lines,
            reason,
            text,
        }
    }
}

/// Exposure as the native controls hold it now: the last value asked for, the value the driver
/// reports in force, and the clamp the driver reported for the last request (`None` when it did not
/// clamp it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureReport {
    /// The exposure asked for last, in microseconds (the initial value before any request).
    pub requested_us: u32,
    /// The exposure the driver reports in force, in microseconds. `None` until a report says, and
    /// when the driver does not report exposure (`NOT_SUPPORTED`).
    pub applied_us: Option<u32>,
    /// The clamp the driver reported for the last request, if it clamped it.
    pub clamp: Option<Clamp>,
}
