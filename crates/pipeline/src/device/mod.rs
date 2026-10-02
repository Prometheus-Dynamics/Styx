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

pub use pisp::{PispFrame, PispOptions, PispPipeline, PispStartup, PispTimes};
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
/// duration together from `r.frame` (the scheduler writes each `delay` frames earlier; what is
/// due in the current frame is written at once while enough of the frame is left, and before
/// streaming the values for frame 0 are written at once). Returns the frame the last of them
/// lands on.
pub fn apply_request(controls: &CameraControls, r: &SensorRequest) -> crate::Result<u64> {
    let landings = controls.request_at_now(
        r.frame,
        &ControlRequest {
            exposure: Some(r.exposure),
            gain: Some(r.analogue_gain),
            frame_duration: Some(r.frame_duration),
        },
    )?;
    Ok(landings.iter().map(|l| l.frame).max().unwrap_or(r.frame))
}

/// Which ISP processes a native camera's frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IspKind {
    /// The Raspberry Pi 5 / CM5 PiSP: the receiver has a `pisp-fe` front end and a `pispbe`
    /// back end is present.
    Pisp,
    /// The software ISP (any raw sensor).
    Software,
}

impl IspKind {
    /// For listings: `pisp` or `software`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pisp => "pisp",
            Self::Software => "software",
        }
    }
}

/// The ISP a camera's frames can go through: the PiSP when its front end is in the camera's
/// media graph and a back end exists, else the software ISP.
pub fn isp_kind(info: &styx_native::CameraInfo) -> IspKind {
    let fe = info.topology.entity_by_name("pisp-fe").is_some();
    if fe && !styx_pisp::device::find_media("pispbe").is_empty() {
        IspKind::Pisp
    } else {
        IspKind::Software
    }
}

/// Directories searched for a sensor's tuning file (the description's `tuning` name): Styx's
/// own, then libcamera's Raspberry Pi ones (read at run time, never copied).
pub const TUNING_DIRS: &[&str] = &[
    "/etc/styx/tuning",
    "/usr/local/share/styx/tuning",
    "/usr/share/styx/tuning",
    "/usr/local/share/libcamera/ipa/rpi/pisp",
    "/usr/share/libcamera/ipa/rpi/pisp",
];

/// Environment variable naming a tuning file to use instead of searching.
pub const TUNING_ENV: &str = "STYX_TUNING";

/// The tuning for a sensor: [`TUNING_ENV`] if set, else the description's `tuning` file in
/// [`TUNING_DIRS`], else the defaults (grey world, built-in metering). Returns where it came
/// from too.
pub fn find_tuning(desc: &styx_sensor::SensorDescription) -> (styx_algo::Tuning, String) {
    let load = |p: &std::path::Path| styx_algo::Tuning::load(p).ok();
    if let Some(p) = std::env::var_os(TUNING_ENV) {
        let p = std::path::PathBuf::from(p);
        if let Some(t) = load(&p) {
            return (t, p.display().to_string());
        }
    }
    if let Some(name) = &desc.sensor.tuning {
        for dir in TUNING_DIRS {
            let p = std::path::Path::new(dir).join(name);
            if p.is_file()
                && let Some(t) = load(&p)
            {
                return (t, p.display().to_string());
            }
        }
    }
    (styx_algo::Tuning::default(), "defaults".into())
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

/// CPU time of each thread of this process: `(name, user, system)` (clock ticks of 10 ms).
pub fn thread_usage() -> Vec<(String, std::time::Duration, std::time::Duration)> {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut out: Vec<_> = dir
        .filter_map(|e| {
            let s = std::fs::read_to_string(e.ok()?.path().join("stat")).ok()?;
            let (head, rest) = s.rsplit_once(')')?;
            let name = head.split_once('(')?.1.to_string();
            let f: Vec<&str> = rest.split_whitespace().collect();
            let tick = |i: usize| -> Option<std::time::Duration> {
                Some(std::time::Duration::from_millis(
                    f.get(i)?.parse::<u64>().ok()? * 10,
                ))
            };
            Some((name, tick(11)?, tick(12)?))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
