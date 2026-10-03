//! The loop on a native camera (feature `device`).
//!
//! * [`SoftPipeline`]: raw frames from the receiver's raw node, processed by the software ISP.
//! * [`PispPipeline`]: raw frames through the PiSP front end (statistics every frame) into the
//!   back end (two outputs), the raw buffers passed as dma-bufs.
//!
//! Both apply the algorithms' sensor requests through the camera's frame-exact control
//! schedule, on the frame each request names.

mod pisp;
mod pisp_options;
mod soft;
mod still_be;

use styx_algo::SensorRequest;
use styx_native::{CameraControls, FrameControls, NativeError};
use styx_sensor::ControlRequest;

pub use pisp::{PispFrame, PispPipeline, PispStartup, PispTimes};
pub use pisp_options::PispOptions;
pub use soft::{SoftFrame, SoftPipeline};
pub use still_be::StillBackEnd;

use crate::controller::SensorValues;
use crate::error::PipelineError;

impl From<NativeError> for PipelineError {
    fn from(e: NativeError) -> Self {
        PipelineError::Device(e.to_string())
    }
}

impl From<styx_pisp::device::DeviceError> for PipelineError {
    fn from(e: styx_pisp::device::DeviceError) -> Self {
        match e {
            styx_pisp::device::DeviceError::OutputsHeld(i) => PipelineError::OutputsHeld(i),
            e => PipelineError::Device(format!("pisp: {e}")),
        }
    }
}

/// Picks the frames whose raw data [`PispPipeline::set_raw_copy`] copies out, from what
/// produced them.
pub type RawCopy = Box<dyn FnMut(&SensorValues) -> bool + Send>;

/// A held copy of a front end raw buffer (16-bit samples, the sensor's value at the top)
/// with the settings `step` processes it with; `None` if the buffer is short.
fn hold_raw(
    data: &[u8],
    format: styx_pisp::uapi::ImageFormatConfig,
    info: &crate::SensorInfo,
    step: &crate::Step,
    values: &SensorValues,
) -> Option<crate::still::HeldRaw> {
    let stride = format.stride.max(0) as usize;
    let len = stride * usize::from(format.height);
    Some(crate::still::HeldRaw {
        sequence: values.frame,
        timestamp: std::time::Duration::ZERO,
        width: u32::from(format.width),
        height: u32::from(format.height),
        stride,
        packing: styx_softisp::RawPacking::U16Le { bits: 16 },
        cfa: info.cfa,
        bits: info.bits,
        data: data.get(..len)?.to_vec(),
        sensor: *values,
        isp: step.isp.clone(),
        params: Box::new(step.params.clone()),
    })
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

/// Environment variable forcing the software ISP (`software`) on cameras with a PiSP.
pub const ISP_ENV: &str = "STYX_NATIVE_ISP";

/// The ISP a camera's frames can go through: the PiSP when its front end is in the camera's
/// media graph and a back end exists, else (or with [`ISP_ENV`]`=software`) the software ISP.
pub fn isp_kind(info: &styx_native::CameraInfo) -> IspKind {
    if std::env::var(ISP_ENV).is_ok_and(|v| v == "software") {
        return IspKind::Software;
    }
    let fe = info.topology.entity_by_name("pisp-fe").is_some();
    if fe && !styx_pisp::device::find_media("pispbe").is_empty() {
        IspKind::Pisp
    } else {
        IspKind::Software
    }
}

/// The dma-heap the software path captures into when it exists (`/dev/dma_heap/linux,cma`).
pub const SOFT_CAPTURE_HEAP: &str = "linux,cma";

/// Where the software ISP path should capture raw frames: cached buffers from
/// [`SOFT_CAPTURE_HEAP`] when the system has that heap, else the driver's MMAP buffers. The
/// CPU reads `rp1-cfe`'s MMAP buffers uncached: on the CM5 a 1280x800 RAW10 frame then costs
/// the software ISP 0.6 ms more per frame than from cached memory (the cache maintenance per
/// frame is included in that comparison).
pub fn soft_capture_memory() -> styx_native::BufferMemory {
    if std::path::Path::new("/dev/dma_heap")
        .join(SOFT_CAPTURE_HEAP)
        .exists()
    {
        styx_native::BufferMemory::DmaHeap(SOFT_CAPTURE_HEAP.into())
    } else {
        styx_native::BufferMemory::Mmap
    }
}

pub use crate::tuning::{TUNING_ENV, find_tuning};

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

/// One thread of this process: name, CPU time (user + system) and context switches
/// (voluntary: it waited; involuntary: it was preempted), for measurements.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadUsage {
    /// Thread id.
    pub tid: u32,
    /// Thread name (`comm`).
    pub name: String,
    /// CPU time (from the scheduler's run time, nanosecond resolution).
    pub cpu: std::time::Duration,
    /// Of that, in the kernel (10 ms ticks).
    pub system: std::time::Duration,
    /// Voluntary context switches (waits, i.e. wakeups).
    pub voluntary: u64,
    /// Involuntary context switches.
    pub involuntary: u64,
}

/// CPU time and context switches of every thread of this process (from `/proc/self/task`).
pub fn thread_usage() -> Vec<ThreadUsage> {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut out: Vec<ThreadUsage> = dir
        .flatten()
        .filter_map(|e| {
            let tid = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            let (head, rest) = stat.rsplit_once(')')?;
            let name = head.split_once('(')?.1.to_string();
            let f: Vec<&str> = rest.split_whitespace().collect();
            let utime = f.get(11)?.parse::<u64>().ok()?;
            let stime = f.get(12)?.parse::<u64>().ok()?;
            let run_ns = std::fs::read_to_string(e.path().join("schedstat"))
                .ok()
                .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok());
            let status = std::fs::read_to_string(e.path().join("status")).unwrap_or_default();
            let field = |k: &str| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(k))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0)
            };
            Some(ThreadUsage {
                tid,
                name,
                cpu: run_ns.map_or(
                    std::time::Duration::from_millis((utime + stime) * 10),
                    std::time::Duration::from_nanos,
                ),
                system: std::time::Duration::from_millis(stime * 10),
                voluntary: field("voluntary_ctxt_switches:"),
                involuntary: field("nonvoluntary_ctxt_switches:"),
            })
        })
        .collect();
    out.sort_by_key(|t| t.tid);
    out
}
