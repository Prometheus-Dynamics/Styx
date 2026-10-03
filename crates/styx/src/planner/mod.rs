//! Frame planning: turn a consumer's [`FrameRequirements`] into a concrete capture, decode and
//! preparation plan for a device.
//!
//! ```no_run
//! use styx::prelude::*;
//! use styx::planner::plan_best;
//!
//! let requirements = FrameRequirements::luma()
//!     .stride_alignment(64)
//!     .pyramid(2)
//!     .min_resolution(1280, 720);
//! let plan = plan_best(&probe_all(), &requirements)?;
//! println!("{plan}"); // every step, where it runs, its estimated cost, and rejected options
//! let mut frames = plan.start()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Candidates are ranked by (1) meeting the requirements, (2) resolution: closest above
//! `min_resolution` when set, else the smallest covering `output_resolution` when set,
//! otherwise largest, (3) estimated cost for the requested
//! [`Priority`], then frame rate. Hardware decoders are only considered when their feature is
//! enabled and they opened successfully at registry creation; [`PlanOverrides`] can force or
//! forbid specific backends, decoders and hardware.
//!
//! Several consumers of one camera (say, a small luma frame for a detector and full-size RGB for
//! a recorder) share one capture: [`plan_many`] picks a mode that serves all of them and plans a
//! branch per consumer.

pub(crate) mod cost;
mod native;
mod routes;
mod session;
pub(crate) use session::SharedSession;
mod shared;
mod start;

use std::fmt;
use std::sync::Arc;

use styx_codec::{CodecRegistry, CodecRegistryHandle};
use styx_core::prelude::*;

pub use cost::StepCost;
pub(crate) use routes::Route;
pub use shared::{SharedFramePlan, plan_many, plan_many_with};
pub use start::{PlannedFrames, RoiHandle};

use crate::BackendKind;
use crate::prelude::{Interval, Mode, ProbedDevice};

/// Where a plan step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepExecution {
    /// No pixels are touched (views, driver buffers).
    ZeroCopy,
    /// A fixed-function block (ISP, hardware decoder, scaler).
    Hardware,
    /// Host CPU.
    Cpu,
}

/// What a plan step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Capture,
    Decode,
    LumaView,
    Pyramid {
        level: u8,
    },
    Crop,
    /// Downscaling in hardware (the ISP).
    Scale,
    /// Compressing frames (H.264, H.265, MJPEG).
    Encode,
}

#[derive(Debug, Clone)]
pub struct PlanStep {
    pub kind: StepKind,
    pub execution: StepExecution,
    pub detail: String,
    pub cost: StepCost,
}

/// A candidate the planner did not choose, and why.
#[derive(Debug, Clone)]
pub struct PlanRejection {
    pub candidate: String,
    pub reason: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PlanError {
    #[error("no devices to plan for")]
    NoDevices,
    #[error("no capture mode satisfies the requirements ({} candidates rejected)", rejected.len())]
    NoCandidates { rejected: Vec<PlanRejection> },
    #[error("no consumers to plan for")]
    NoConsumers,
    #[error("codec registry unavailable: {0}")]
    Registry(String),
}

/// A concrete way to deliver frames that meet a [`FrameRequirements`].
#[derive(Clone)]
pub struct FramePlan {
    pub device: ProbedDevice,
    pub backend: BackendKind,
    pub mode: Mode,
    pub interval: Option<Interval>,
    pub requirements: FrameRequirements,
    pub steps: Vec<PlanStep>,
    /// Estimated cost per frame across all steps.
    pub total: StepCost,
    pub notes: Vec<String>,
    pub rejected: Vec<PlanRejection>,
    pub(crate) route: Route,
    pub(crate) isp_pyramid_level: Option<u8>,
    pub(crate) decode_scale: u8,
    pub(crate) isp_output: Option<(u32, u32)>,
    /// On a shared capture: the ISP output this consumer takes is in this format, not the
    /// capture mode's (native PiSP).
    pub(crate) isp_format: Option<FourCc>,
    /// On a shared capture: frames come from the ISP's second output (at `isp_output`).
    pub(crate) isp_second_output: bool,
    /// Frames the preparer makes go into memfd buffers other processes can map.
    pub(crate) exportable: bool,
    pub(crate) decode_threads: usize,
    pub(crate) queue_depth: usize,
    pub(crate) stop_when_idle: Option<(std::time::Duration, crate::capture_api::IdleStop)>,
    /// Buffers to capture into when frames pass through unchanged.
    #[cfg(target_os = "linux")]
    pub(crate) capture_buffers: Option<crate::capture_api::CaptureBuffers>,
}

impl fmt::Debug for FramePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FramePlan")
            .field("device", &self.device.identity.display)
            .field("backend", &self.backend)
            .field("mode", &self.mode.format)
            .field("steps", &self.steps)
            .field("total", &self.total)
            .finish()
    }
}

impl fmt::Display for FramePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = self.mode.format.resolution;
        writeln!(
            f,
            "frame plan for {} via {} {} {}x{}{}: ~{:.1} ms latency, ~{:.2} ms CPU per frame",
            self.device.identity.display,
            routes::backend_name(self.backend),
            self.mode.format.code,
            res.width,
            res.height,
            self.interval
                .map(|i| format!(" @ {:.0} fps", i.fps()))
                .unwrap_or_default(),
            self.total.latency_ms,
            self.total.cpu_ms,
        )?;
        for (index, step) in self.steps.iter().enumerate() {
            let kind = match step.kind {
                StepKind::Capture => "capture".to_string(),
                StepKind::Decode => "decode".to_string(),
                StepKind::LumaView => "luma view".to_string(),
                StepKind::Pyramid { level } => format!("pyramid L{level}"),
                StepKind::Crop => "roi".to_string(),
                StepKind::Scale => "scale".to_string(),
                StepKind::Encode => "encode".to_string(),
            };
            let execution = match step.execution {
                StepExecution::ZeroCopy => "zero-copy",
                StepExecution::Hardware => "hardware",
                StepExecution::Cpu => "cpu",
            };
            writeln!(
                f,
                "  {}. {kind:<10} {execution:<9} {:>6.2} ms  {}",
                index + 1,
                step.cost.latency_ms,
                step.detail
            )?;
        }
        for note in &self.notes {
            writeln!(f, "  note: {note}")?;
        }
        if !self.rejected.is_empty() {
            let shown = self.rejected.len().min(8);
            writeln!(
                f,
                "  rejected ({} total, first {shown}):",
                self.rejected.len()
            )?;
            for rejection in self.rejected.iter().take(shown) {
                writeln!(f, "    - {}: {}", rejection.candidate, rejection.reason)?;
            }
        }
        Ok(())
    }
}

fn default_registry() -> Result<CodecRegistryHandle, PlanError> {
    CodecRegistry::with_enabled_codecs()
        .map(|registry| registry.handle())
        .map_err(|err| PlanError::Registry(err.to_string()))
}

/// Plan for one device using the default codec registry.
pub fn plan_frames(
    device: &ProbedDevice,
    requirements: &FrameRequirements,
) -> Result<FramePlan, PlanError> {
    plan_frames_with(device, requirements, &default_registry()?)
}

/// Plan for one device using `registry` to find decoders.
pub fn plan_frames_with(
    device: &ProbedDevice,
    requirements: &FrameRequirements,
    registry: &CodecRegistryHandle,
) -> Result<FramePlan, PlanError> {
    plan_devices(std::slice::from_ref(device), requirements, registry)
}

/// Plan across `devices`, choosing the best camera as well as its mode and route.
pub fn plan_best(
    devices: &[ProbedDevice],
    requirements: &FrameRequirements,
) -> Result<FramePlan, PlanError> {
    plan_devices(devices, requirements, &default_registry()?)
}

fn plan_devices(
    devices: &[ProbedDevice],
    req: &FrameRequirements,
    registry: &CodecRegistryHandle,
) -> Result<FramePlan, PlanError> {
    if devices.is_empty() {
        return Err(PlanError::NoDevices);
    }
    let mut rejected = Vec::new();
    let mut best: Option<(RankKey, &ProbedDevice, routes::Candidate<'_>)> = None;
    for device in devices {
        for candidate in routes::candidates(device, req, registry, &mut rejected) {
            let key = rank_key(&candidate, req);
            if best.as_ref().is_none_or(|(best_key, _, _)| key < *best_key) {
                best = Some((key, device, candidate));
            }
        }
    }
    let Some((_, device, chosen)) = best else {
        return Err(PlanError::NoCandidates { rejected });
    };
    let interval = pick_interval(&chosen.mode, req);
    Ok(plan_from(device, chosen, req, interval, rejected))
}

/// The plan for `req` from its chosen candidate.
pub(crate) fn plan_from(
    device: &ProbedDevice,
    chosen: routes::Candidate<'_>,
    req: &FrameRequirements,
    interval: Option<Interval>,
    rejected: Vec<PlanRejection>,
) -> FramePlan {
    FramePlan {
        device: device.clone(),
        backend: chosen.backend.kind,
        mode: chosen.mode,
        interval,
        requirements: req.clone(),
        steps: chosen.steps,
        total: chosen.total,
        notes: chosen.notes,
        rejected,
        route: chosen.route,
        isp_pyramid_level: chosen.isp_pyramid_level,
        decode_scale: chosen.decode_scale,
        isp_output: chosen.isp_output,
        isp_format: chosen.isp_format,
        isp_second_output: false,
        exportable: false,
        decode_threads: cost::decode_threads(req.priority, req.overrides.decode_threads),
        queue_depth: cost::queue_depth(req.priority, req.overrides.queue_depth),
        stop_when_idle: None,
        #[cfg(target_os = "linux")]
        capture_buffers: None,
    }
}

/// Lexicographic rank; smaller is better.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub(crate) struct RankKey {
    pub(crate) resolution: f64,
    pub(crate) score: f32,
    pub(crate) fps: f32,
    pub(crate) backend: u8,
    /// Consumers served by an ISP output in another format than the mode's (all else equal,
    /// the mode that is the format wins).
    pub(crate) isp_formats: u8,
}

fn rank_key(candidate: &routes::Candidate<'_>, req: &FrameRequirements) -> RankKey {
    let res = candidate.mode.format.resolution;
    let area = f64::from(res.width.get()) * f64::from(res.height.get());
    let covers_output = req
        .output_resolution
        .map(|(w, h)| res.width.get() >= w && res.height.get() >= h);
    // Size of the frames the route delivers, after the ISP or the decoder scales them.
    let scale = u32::from(candidate.decode_scale.max(1));
    let (dw, dh) = candidate.isp_output.unwrap_or((
        res.width.get().div_ceil(scale),
        res.height.get().div_ceil(scale),
    ));
    RankKey {
        // With a minimum: the smallest mode that satisfies it. With an output size: the route
        // delivering the smallest frames that cover it (so an ISP scaling a wide mode beats a
        // smaller mode of another aspect ratio), else the largest mode. Otherwise: the largest.
        resolution: match (req.min_resolution, covers_output) {
            (Some(_), _) => area,
            (None, Some(true)) => f64::from(dw) * f64::from(dh),
            // Ranked after every covering mode (areas are far below 1e15).
            (None, Some(false)) => 1e15 - area,
            (None, None) => -area,
        },
        score: cost::score(candidate.total, req.priority),
        fps: -candidate.fps.unwrap_or(0.0),
        // Prefer V4L2 over libcamera's UVC pipeline for the same USB camera; ISP paths already
        // win on cost.
        backend: match candidate.backend.kind {
            BackendKind::V4l2 => 0,
            _ => 1,
        },
        isp_formats: u8::from(candidate.isp_format.is_some()),
    }
}

/// The frame rate a plan runs at when no consumer asks for one, on a mode that can run at any
/// rate in a range (a sensor Styx drives): 30 fps, or the nearest rate the mode allows. The
/// fastest such a mode allows is rarely wanted (640x400 on the OV9782: 260 fps, its exposure
/// limited to 3.8 ms). Modes with a list of rates (UVC cameras) keep the list's fastest.
pub const DEFAULT_FPS: u32 = 30;

/// [`DEFAULT_FPS`] within `mode`'s rate range; `None` for modes without one.
pub(crate) fn default_interval(mode: &Mode) -> Option<Interval> {
    let range = mode.interval_stepwise?;
    let want = Interval::from_fps(DEFAULT_FPS)?;
    Some(if want.fps() > range.min.fps() {
        range.min
    } else if want.fps() < range.max.fps() {
        range.max
    } else {
        want
    })
}

/// No `min_fps`: [`DEFAULT_FPS`] on a mode with a rate range. Otherwise, and on modes with a
/// list of rates, the fastest (or with `Priority::Power` the slowest meeting `min_fps`,
/// exactly `min_fps` where the mode has a range).
fn pick_interval(mode: &Mode, req: &FrameRequirements) -> Option<Interval> {
    if req.min_fps.is_none()
        && let Some(default) = default_interval(mode)
    {
        return Some(default);
    }
    let fastest = mode
        .intervals
        .iter()
        .copied()
        .max_by(|a, b| a.fps().total_cmp(&b.fps()));
    // A mode that can run at any rate in a range (a sensor Styx drives) runs at exactly the
    // rate asked for when saving power.
    let exact = req
        .min_fps
        .and_then(Interval::from_fps)
        .filter(|i| mode.interval_stepwise.is_some_and(|s| s.contains(*i)));
    match req.priority {
        Priority::Power if exact.is_some() => exact,
        Priority::Power => mode
            .intervals
            .iter()
            .copied()
            .filter(|i| req.min_fps.is_none_or(|min| i.fps() + 0.5 >= min as f32))
            .min_by(|a, b| a.fps().total_cmp(&b.fps()))
            .or(fastest),
        _ => fastest,
    }
}

impl FramePlan {
    /// Whether any step runs on a hardware block.
    pub fn uses_hardware(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.execution == StepExecution::Hardware)
    }

    /// Size of the frames delivered (before any region of interest): the capture size, or
    /// smaller when the ISP or the decoder scales toward
    /// [`FrameRequirements::output_resolution`].
    pub fn output_resolution(&self) -> (u32, u32) {
        if let Some(size) = self.isp_output {
            return size;
        }
        let res = self.mode.format.resolution;
        let scale = u32::from(self.decode_scale.max(1));
        (
            res.width.get().div_ceil(scale),
            res.height.get().div_ceil(scale),
        )
    }

    /// Stop the camera streaming after `after` without a pull, and start it again on the next
    /// (libcamera and V4L2; see `StyxConfig::stop_when_idle`).
    pub fn stop_when_idle(mut self, after: std::time::Duration) -> Self {
        self.stop_when_idle = Some((after, crate::capture_api::IdleStop::Release));
        self
    }

    /// Put frames the plan decodes or copies into memfd buffers, so `styx::ipc` can pass them to
    /// other processes without copying (Linux). Camera buffers are shareable already.
    pub fn exportable(mut self) -> Self {
        self.exportable = true;
        self
    }

    /// Capture into `buffers` (see [`crate::capture_api::import`]) when the plan delivers the
    /// camera's frames unchanged and the backend can; otherwise frames come in the plan's own
    /// buffers and [`crate::capture_api::CaptureBuffers::in_use`] stays false.
    #[cfg(target_os = "linux")]
    pub fn capture_into(mut self, buffers: crate::capture_api::CaptureBuffers) -> Self {
        self.capture_buffers = Some(buffers);
        self
    }

    /// Like [`FramePlan::stop_when_idle`], but keep the camera configured while idle so it
    /// starts again quickly (libcamera; see `IdleStop::Pause`).
    pub fn pause_when_idle(mut self, after: std::time::Duration) -> Self {
        self.stop_when_idle = Some((after, crate::capture_api::IdleStop::Pause));
        self
    }

    /// Decode threads per frame (0 = automatic), from the priority or an override.
    pub fn decode_threads(&self) -> usize {
        self.decode_threads
    }

    /// Frames buffered between capture and consumer, from the priority or an override.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }

    /// The decoder chosen for this plan, if the capture format needs one.
    pub fn decoder(&self) -> Option<Arc<dyn styx_codec::Codec>> {
        match &self.route {
            Route::Decode { decoder, .. } => Some(decoder.clone()),
            Route::Encode { decoder, .. } => decoder.clone(),
            _ => None,
        }
    }

    /// The encoder chosen for this plan, when the consumer wants compressed frames the camera
    /// does not produce.
    pub fn encoder(&self) -> Option<Arc<dyn styx_codec::Codec>> {
        match &self.route {
            Route::Encode { encoder, .. } => Some(encoder.clone()),
            _ => None,
        }
    }

    /// Whether frames are inter-coded packets (H.264/H.265): only keyframes stand alone.
    pub fn inter_coded(&self) -> bool {
        let output = match &self.route {
            Route::Encode { encoder, .. } => encoder.descriptor().output,
            Route::Direct => self.mode.format.code,
            _ => return false,
        };
        matches!(output, FourCc::H264 | FourCc::H265 | FourCc::HEVC)
    }
}

#[cfg(test)]
mod tests;
