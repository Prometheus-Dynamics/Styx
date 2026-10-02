//! The loop on a native camera (feature `device`).
//!
//! * [`SoftPipeline`]: raw frames from the receiver's raw node, processed by the software ISP.
//! * [`PispPipeline`]: raw frames through the PiSP front end (statistics every frame) into the
//!   back end (two outputs), the raw buffers passed as dma-bufs.
//!
//! Both apply the algorithms' sensor requests through the camera's frame-exact control
//! schedule, on the frame each request names.

mod pisp;
mod soft;

use styx_algo::SensorRequest;
use styx_native::{CameraControls, FrameControls, NativeError};
use styx_sensor::ControlRequest;

pub use pisp::{PispFrame, PispOptions, PispPipeline, PispTimes};
pub use soft::{SoftFrame, SoftPipeline};

use crate::controller::SensorValues;
use crate::error::PipelineError;

impl From<NativeError> for PipelineError {
    fn from(e: NativeError) -> Self {
        PipelineError::Device(e.to_string())
    }
}

impl From<styx_pisp::device::DeviceError> for PipelineError {
    fn from(e: styx_pisp::device::DeviceError) -> Self {
        PipelineError::Device(format!("pisp: {e}"))
    }
}

/// What produced frame `sequence`, as the camera reports it.
pub fn sensor_values(sequence: u64, c: &FrameControls) -> SensorValues {
    SensorValues {
        frame: sequence,
        exposure: c.exposure,
        analogue_gain: c.analog_gain,
        digital_gain: c.digital_gain,
        frame_duration: c.frame_duration,
        verified: c.verified,
    }
}

/// Hands a sensor request to the camera's control schedule: exposure, analogue gain and frame
/// duration together from `r.frame` (the scheduler writes each `delay` frames earlier).
/// Returns the frame the last of them lands on.
pub fn apply_request(controls: &CameraControls, r: &SensorRequest) -> crate::Result<u64> {
    let landings = controls.request_at(
        r.frame,
        &ControlRequest {
            exposure: Some(r.exposure),
            gain: Some(r.analogue_gain),
            frame_duration: Some(r.frame_duration),
        },
    )?;
    Ok(landings.iter().map(|l| l.frame).max().unwrap_or(r.frame))
}

/// CPU time of this process (user + system) and its peak resident set, for measurements.
pub fn process_usage() -> (std::time::Duration, u64) {
    let cpu = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let rest = s.rsplit_once(')')?.1;
            let f: Vec<&str> = rest.split_whitespace().collect();
            // Fields after the command: state is 3rd overall, utime 14th, stime 15th.
            let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
            Some(std::time::Duration::from_millis(ticks * 10))
        })
        .unwrap_or_default();
    let hwm = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .unwrap_or(0);
    (cpu, hwm * 1024)
}
