//! Frame planning: turn a consumer's [`FrameRequest`] into a concrete capture, decode and
//! preparation plan for a camera, and run it.
//!
//! ```no_run
//! use styx::prelude::*;
//!
//! let request = Frames::nv12().size(1280, 800).fps(30);
//! let plan = request.plan_best(&probe_all())?;
//! println!("{plan}"); // every step, where it runs, its estimated cost, and rejected options
//! let mut frames = plan.start()?; // or request.open_best(&probe_all())? in one go
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! For every camera, backend and mode the planner builds the route to the format asked for
//! (zero-copy where it can, the ISP or a hardware decoder where there is one, else the CPU),
//! drops those that cannot meet the request (size bounds, frame rate, route choices), and ranks
//! the rest by (1) size: the smallest mode at or above [`FrameRequest::size_at_least`] when
//! set, else the route delivering the smallest frames that cover [`FrameRequest::size`], else
//! the largest mode; (2) cost: CPU time plus half the latency per frame (see [`StepCost`]),
//! which favours hardware blocks; (3) the fastest mode. Hardware decoders are only considered
//! when their feature is enabled and they opened at registry creation.
//!
//! Several consumers of one camera (say, a small grey frame for a detector and full-size RGB
//! for a recorder) share one capture: [`plan_many`] picks a mode and a rate that serve all of
//! them and plans a branch per consumer.

pub(crate) mod cost;
mod delivered;
mod frames;
mod native;
mod pyramid;
mod rate;
mod region;
mod region_frames;
mod region_shared;
mod region_soft;
mod request;
mod roi;
mod routes;
mod session;
pub(crate) use session::{SharedSession, same_plan};
mod shared;
mod start;

use std::fmt;
use std::sync::Arc;

use styx_codec::{CodecRegistry, CodecRegistryHandle};
use styx_core::prelude::*;

pub use cost::StepCost;
pub use delivered::{Delivered, Unmet};
#[cfg(any(feature = "native", feature = "uvc"))]
pub(crate) use rate::default_interval;
pub use region::RoiCrop;
#[cfg(feature = "native")]
pub(crate) use region::{soft_overview_factor, soft_overview_size};

pub use request::{
    CameraFrames, Delivery, FrameRate, FrameRequest, Hardware, MAX_REGIONS, OpenError,
};
pub(crate) use routes::Route;
pub use shared::{SharedFramePlan, plan_many, plan_many_with};
#[allow(deprecated)]
pub use start::PlannedFrames;
pub use start::{Frames, RoiHandle};

use crate::BackendKind;
use crate::capture_api::StyxConfig;
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
    #[error("no capture mode delivers the frames asked for ({} candidates rejected)", rejected.len())]
    NoCandidates { rejected: Vec<PlanRejection> },
    /// No mode that could deliver the frames runs at the rate asked for; the message names the
    /// rates there are.
    #[error("{0}")]
    FrameRate(String),
    #[error("no consumers to plan for")]
    NoConsumers,
    #[error("codec registry unavailable: {0}")]
    Registry(String),
}

/// A concrete way to deliver the frames a [`FrameRequest`] asks for.
#[derive(Clone)]
pub struct FramePlan {
    pub device: ProbedDevice,
    pub backend: BackendKind,
    pub mode: Mode,
    pub interval: Option<Interval>,
    pub request: FrameRequest,
    pub steps: Vec<PlanStep>,
    /// Estimated cost per frame across all steps.
    pub total: StepCost,
    pub notes: Vec<String>,
    /// What of the request the frames do not meet ([`FramePlan::delivered`]); empty when all is.
    pub unmet: Vec<Unmet>,
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
    /// How the region of interest and overview reach the frames.
    pub(crate) region: region::Region,
    /// Frames the preparer makes go into memfd buffers other processes can map.
    pub(crate) exportable: bool,
    pub(crate) decode_threads: usize,
    pub(crate) queue_depth: usize,
    pub(crate) stop_when_idle: Option<(std::time::Duration, crate::capture_api::IdleStop)>,
    /// Capture settings to start from ([`FramePlan::config`]).
    pub(crate) config: Option<StyxConfig>,
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
        writeln!(
            f,
            "  cost {:.2} = CPU + latency x {}: the cheapest route to frames of this size",
            cost::score(self.total),
            cost::LATENCY_WEIGHT
        )?;
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

/// Plan for one device using the default codec registry. Takes a [`FrameRequest`] (or the
/// deprecated `FrameRequirements`); [`FrameRequest::plan`] is the same.
pub fn plan_frames<R: Clone + Into<FrameRequest>>(
    device: &ProbedDevice,
    request: &R,
) -> Result<FramePlan, PlanError> {
    plan_frames_with(device, request, &default_registry()?)
}

/// Plan for one device using `registry` to find decoders.
pub fn plan_frames_with<R: Clone + Into<FrameRequest>>(
    device: &ProbedDevice,
    request: &R,
    registry: &CodecRegistryHandle,
) -> Result<FramePlan, PlanError> {
    plan_devices(
        std::slice::from_ref(device),
        &request.clone().into(),
        registry,
    )
}

/// Plan across `devices`, choosing the best camera as well as its mode and route
/// ([`FrameRequest::plan_best`]).
pub fn plan_best<R: Clone + Into<FrameRequest>>(
    devices: &[ProbedDevice],
    request: &R,
) -> Result<FramePlan, PlanError> {
    plan_devices(devices, &request.clone().into(), &default_registry()?)
}

fn plan_devices(
    devices: &[ProbedDevice],
    req: &FrameRequest,
    registry: &CodecRegistryHandle,
) -> Result<FramePlan, PlanError> {
    if devices.is_empty() {
        return Err(PlanError::NoDevices);
    }
    let mut rejected = Vec::new();
    let mut best: Option<(RankKey, &ProbedDevice, routes::Candidate<'_>)> = None;
    for device in devices {
        for candidate in routes::candidates(device, req, registry, &mut rejected) {
            let unmet = candidate.unmet(req);
            if req.strict && !unmet.is_empty() {
                rejected.push(PlanRejection {
                    candidate: routes::describe(candidate.backend, &candidate.mode),
                    reason: strict_reason(&unmet),
                });
                continue;
            }
            let key = rank_key(&candidate, req);
            if best.as_ref().is_none_or(|(best_key, _, _)| key < *best_key) {
                best = Some((key, device, candidate));
            }
        }
    }
    let Some((_, device, chosen)) = best else {
        return Err(no_candidates(req.fps, rejected));
    };
    let interval = rate::pick(&chosen.mode, req.fps);
    Ok(plan_from(device, chosen, req, interval, rejected))
}

/// The error when nothing meets the request: about the rate when that is all that stopped some
/// mode, naming the rates there are.
/// Why a strict request rejects a candidate.
pub(crate) fn strict_reason(unmet: &[Unmet]) -> String {
    let unmet: Vec<String> = unmet.iter().map(ToString::to_string).collect();
    format!("strict: {}", unmet.join("; "))
}

pub(crate) fn no_candidates(fps: FrameRate, rejected: Vec<PlanRejection>) -> PlanError {
    // Modes by the rates they run at: "30, 25, 15 fps (v4l2 YUYV 640x480 and 3 more)".
    let mut rates: Vec<(&str, Vec<&str>)> = Vec::new();
    for r in rejected
        .iter()
        .filter(|r| r.reason.starts_with(rate::REJECTED))
    {
        let runs = r
            .reason
            .split_once(": runs at ")
            .map_or("", |(_, runs)| runs);
        match rates.iter_mut().find(|(rates, _)| *rates == runs) {
            Some((_, modes)) => modes.push(&r.candidate),
            None => rates.push((runs, vec![&r.candidate])),
        }
    }
    if rates.is_empty() {
        return PlanError::NoCandidates { rejected };
    }
    let asked = match fps {
        FrameRate::Exactly(fps) => format!("exactly {fps} fps"),
        FrameRate::AtLeast(fps) => format!("at least {fps} fps"),
        FrameRate::Between(min, max) => format!("{min} to {max} fps"),
        FrameRate::CameraDefault => "its default rate".into(),
    };
    let list: Vec<String> = rates
        .iter()
        .take(6)
        .map(|(runs, modes)| match modes.len() {
            1 => format!("{runs} ({})", modes[0]),
            n => format!("{runs} ({} and {} more)", modes[0], n - 1),
        })
        .collect();
    PlanError::FrameRate(format!(
        "no mode delivering these frames runs at {asked}; they run at {}",
        list.join("; ")
    ))
}

/// The plan for `req` from its chosen candidate.
pub(crate) fn plan_from(
    device: &ProbedDevice,
    chosen: routes::Candidate<'_>,
    req: &FrameRequest,
    interval: Option<Interval>,
    rejected: Vec<PlanRejection>,
) -> FramePlan {
    let unmet = chosen.unmet(req);
    FramePlan {
        device: device.clone(),
        backend: chosen.backend.kind,
        mode: chosen.mode,
        interval,
        request: req.clone(),
        steps: chosen.steps,
        total: chosen.total,
        notes: chosen.notes,
        unmet,
        rejected,
        route: chosen.route,
        isp_pyramid_level: chosen.isp_pyramid_level,
        decode_scale: chosen.decode_scale,
        isp_output: chosen.isp_output,
        isp_format: chosen.isp_format,
        region: chosen.region,
        isp_second_output: false,
        exportable: false,
        decode_threads: req.threads(),
        queue_depth: req.delivery.queue_depth(),
        stop_when_idle: None,
        config: None,
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

fn rank_key(candidate: &routes::Candidate<'_>, req: &FrameRequest) -> RankKey {
    let res = candidate.mode.format.resolution;
    let area = f64::from(res.width.get()) * f64::from(res.height.get());
    let covers_output = req
        .size
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
        resolution: match (req.min_size, covers_output) {
            (Some(_), _) => area,
            (None, Some(true)) => f64::from(dw) * f64::from(dh),
            // Ranked after every covering mode (areas are far below 1e15).
            (None, Some(false)) => 1e15 - area,
            (None, None) => -area,
        },
        score: cost::score(candidate.total),
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

/// The frame rate a camera that runs at any rate in a range (a sensor Styx drives) runs at when
/// no rate is asked for: 30 fps, or the nearest rate the mode allows. The fastest such a mode
/// allows is rarely wanted (640x400 on the OV9782: 260 fps, its exposure limited to 3.8 ms).
/// Cameras with a list of rates (USB cameras) run at the listed rate closest to it.
pub const DEFAULT_FPS: u32 = 30;

impl FramePlan {
    /// Whether any step runs on a hardware block.
    pub fn uses_hardware(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.execution == StepExecution::Hardware)
    }

    /// Size of the frames delivered (before any region of interest): the capture size, or
    /// smaller when the ISP or the decoder scales toward [`FrameRequest::size`].
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

    /// Capture settings to start from, e.g. the ISP's denoise
    /// (`StyxConfig::native_temporal_denoise`). The plan sets what it decided on top (queue
    /// depth, ISP outputs, idle stop).
    pub fn config(mut self, config: StyxConfig) -> Self {
        self.config = Some(config);
        self
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

    /// Decode threads per frame (0 = automatic), from the delivery or
    /// [`FrameRequest::decode_threads`].
    pub fn decode_threads(&self) -> usize {
        self.decode_threads
    }

    /// Frames buffered between capture and consumer, from [`FrameRequest::delivery`].
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

    /// Whether frames are the sensor's raw stream (raw Bayer, or GREY/R8 from libcamera's raw
    /// role on a Raspberry Pi camera), not an ISP's processed output.
    pub fn raw_sensor_stream(&self) -> bool {
        let code = self.mode.format.code;
        self.device
            .backends
            .iter()
            .find(|b| b.kind == self.backend)
            .map_or(native::raw_bayer(code), |b| {
                native::raw_sensor_stream(b, code)
            })
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
#[cfg(feature = "native")]
mod region_tests;
#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod tests;
