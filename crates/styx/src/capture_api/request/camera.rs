use styx_capture::prelude::*;

use crate::{BackendKind, ProbedDevice};

use super::{
    CaptureError, CaptureRequest, CaptureStartPolicy, StyxConfig, TdnOutputMode, default_interval,
};

const DEFAULT_CAMERA_FORMATS: &[FourCc] = &[
    FourCc::RG24,
    FourCc::new(*b"RGB3"),
    FourCc::BGR3,
    FourCc::BG24,
    FourCc::RGBA,
    FourCc::BGRA,
    FourCc::NV12,
    FourCc::YUYV,
    FourCc::I420,
    FourCc::MJPG,
    FourCc::JPEG,
    FourCc::R8,
    FourCc::GREY,
];

const DEFAULT_CAMERA_BACKEND_PRIORITY: &[BackendKind] = &[
    BackendKind::V4l2,
    BackendKind::Libcamera,
    BackendKind::Virtual,
    BackendKind::Netcam,
    BackendKind::File,
    BackendKind::Simulation,
];

pub trait CameraFormat {
    fn try_into_fourcc(self) -> Result<FourCc, String>;
}

impl CameraFormat for FourCc {
    fn try_into_fourcc(self) -> Result<FourCc, String> {
        Ok(self)
    }
}

impl CameraFormat for [u8; 4] {
    fn try_into_fourcc(self) -> Result<FourCc, String> {
        Ok(FourCc::new(self))
    }
}

impl CameraFormat for &[u8; 4] {
    fn try_into_fourcc(self) -> Result<FourCc, String> {
        Ok(FourCc::new(*self))
    }
}

impl CameraFormat for &str {
    fn try_into_fourcc(self) -> Result<FourCc, String> {
        self.parse()
    }
}

impl CameraFormat for String {
    fn try_into_fourcc(self) -> Result<FourCc, String> {
        self.as_str().try_into_fourcc()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraIntervalPreference {
    Default,
    Fastest,
    Slowest,
    Exact(Interval),
    None,
}

pub type CameraStartPolicy = CaptureStartPolicy;

/// High-level camera selector for the common "open the best usable camera" path.
///
/// `CameraRequest` keeps the same control as `CaptureRequest`, but moves probing,
/// backend choice, mode choice, and interval choice into one reusable policy.
#[derive(Debug, Clone)]
pub struct CameraRequest {
    devices: Vec<ProbedDevice>,
    format_priority: Vec<FourCc>,
    backend_priority: Vec<BackendKind>,
    resolution_priority: Vec<(u32, u32)>,
    min_width: Option<u32>,
    min_height: Option<u32>,
    max_width: Option<u32>,
    max_height: Option<u32>,
    interval_preference: CameraIntervalPreference,
    controls: Vec<(ControlId, ControlValue)>,
    tdn_output_mode: TdnOutputMode,
    config: Option<StyxConfig>,
}

impl CameraRequest {
    pub fn new() -> Self {
        Self {
            devices: crate::probe_all(),
            format_priority: DEFAULT_CAMERA_FORMATS.to_vec(),
            backend_priority: DEFAULT_CAMERA_BACKEND_PRIORITY.to_vec(),
            resolution_priority: Vec::new(),
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            interval_preference: CameraIntervalPreference::Fastest,
            controls: Vec::new(),
            tdn_output_mode: TdnOutputMode::default(),
            config: None,
        }
    }

    pub fn from_devices(devices: Vec<ProbedDevice>) -> Self {
        Self {
            devices,
            ..Self::empty()
        }
    }

    fn empty() -> Self {
        Self {
            devices: Vec::new(),
            format_priority: DEFAULT_CAMERA_FORMATS.to_vec(),
            backend_priority: DEFAULT_CAMERA_BACKEND_PRIORITY.to_vec(),
            resolution_priority: Vec::new(),
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            interval_preference: CameraIntervalPreference::Fastest,
            controls: Vec::new(),
            tdn_output_mode: TdnOutputMode::default(),
            config: None,
        }
    }

    pub fn format_priority<T: Into<FourCc>>(
        mut self,
        formats: impl IntoIterator<Item = T>,
    ) -> Self {
        self.format_priority = formats.into_iter().map(Into::into).collect();
        self
    }

    pub fn try_format_priority<T: CameraFormat>(
        mut self,
        formats: impl IntoIterator<Item = T>,
    ) -> Result<Self, CaptureError> {
        self.format_priority = formats
            .into_iter()
            .map(CameraFormat::try_into_fourcc)
            .collect::<Result<Vec<_>, _>>()
            .map_err(CaptureError::InvalidConfig)?;
        Ok(self)
    }

    pub fn backend_priority(mut self, priority: impl IntoIterator<Item = BackendKind>) -> Self {
        self.backend_priority = priority.into_iter().collect();
        self
    }

    pub fn resolution_priority(mut self, priority: impl IntoIterator<Item = (u32, u32)>) -> Self {
        self.resolution_priority = priority.into_iter().collect();
        self
    }

    pub fn min_resolution(mut self, width: u32, height: u32) -> Self {
        self.min_width = Some(width);
        self.min_height = Some(height);
        self
    }

    pub fn max_resolution(mut self, width: u32, height: u32) -> Self {
        self.max_width = Some(width);
        self.max_height = Some(height);
        self
    }

    pub fn interval_preference(mut self, preference: CameraIntervalPreference) -> Self {
        self.interval_preference = preference;
        self
    }

    pub fn fastest_interval(mut self) -> Self {
        self.interval_preference = CameraIntervalPreference::Fastest;
        self
    }

    pub fn slowest_interval(mut self) -> Self {
        self.interval_preference = CameraIntervalPreference::Slowest;
        self
    }

    pub fn default_interval(mut self) -> Self {
        self.interval_preference = CameraIntervalPreference::Default;
        self
    }

    pub fn exact_interval(mut self, interval: Interval) -> Self {
        self.interval_preference = CameraIntervalPreference::Exact(interval);
        self
    }

    pub fn no_interval(mut self) -> Self {
        self.interval_preference = CameraIntervalPreference::None;
        self
    }

    pub fn control(mut self, id: ControlId, value: ControlValue) -> Self {
        self.controls.push((id, value));
        self
    }

    pub fn enable_tdn_output(mut self, enable: bool) -> Self {
        self.tdn_output_mode = if enable {
            TdnOutputMode::Force
        } else {
            TdnOutputMode::Off
        };
        self
    }

    pub fn tdn_output_mode(mut self, mode: TdnOutputMode) -> Self {
        self.tdn_output_mode = mode;
        self
    }

    pub fn config(mut self, config: StyxConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn select(&self) -> Result<SelectedCamera, CaptureError> {
        self.select_all()?
            .into_iter()
            .next()
            .ok_or(CaptureError::NoCameraMatchingRequest)
    }

    pub fn select_all(&self) -> Result<Vec<SelectedCamera>, CaptureError> {
        let mut candidates = self.candidates();
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.score()));
        let mut seen_devices = std::collections::HashSet::new();
        let mut selected = Vec::new();
        for candidate in candidates {
            if !seen_devices.insert(candidate.device_index) {
                continue;
            }
            selected.push(SelectedCamera {
                device: self.devices[candidate.device_index].clone(),
                backend: candidate.backend,
                mode: candidate.mode,
                interval: candidate.interval,
                controls: self.controls.clone(),
                tdn_output_mode: self.tdn_output_mode,
                config: self.config.clone(),
            });
        }
        if selected.is_empty() {
            Err(CaptureError::NoCameraMatchingRequest)
        } else {
            Ok(selected)
        }
    }

    pub fn select_many(&self, count: usize) -> Result<Vec<SelectedCamera>, CaptureError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let selected: Vec<_> = self.select_all()?.into_iter().take(count).collect();
        if selected.is_empty() {
            Err(CaptureError::NoCameraMatchingRequest)
        } else {
            Ok(selected)
        }
    }

    pub fn start(self) -> Result<super::super::handle::CaptureHandle, CaptureError> {
        self.start_with_policy(CaptureStartPolicy::default())
    }

    pub fn start_with_policy(
        self,
        policy: CaptureStartPolicy,
    ) -> Result<super::super::handle::CaptureHandle, CaptureError> {
        self.select()?.start_with_policy(policy)
    }

    pub fn start_all(self) -> Result<Vec<super::super::handle::CaptureHandle>, CaptureError> {
        self.start_all_with_policy(CaptureStartPolicy::default())
    }

    pub fn start_many(
        self,
        count: usize,
    ) -> Result<Vec<super::super::handle::CaptureHandle>, CaptureError> {
        self.start_many_with_policy(count, CaptureStartPolicy::default())
    }

    pub fn start_all_with_policy(
        self,
        policy: CaptureStartPolicy,
    ) -> Result<Vec<super::super::handle::CaptureHandle>, CaptureError> {
        self.select_all()?
            .into_iter()
            .map(|selected| selected.start_with_policy(policy))
            .collect()
    }

    pub fn start_many_with_policy(
        self,
        count: usize,
        policy: CaptureStartPolicy,
    ) -> Result<Vec<super::super::handle::CaptureHandle>, CaptureError> {
        self.select_many(count)?
            .into_iter()
            .map(|selected| selected.start_with_policy(policy))
            .collect()
    }

    fn candidates(&self) -> Vec<CameraCandidate> {
        let mut candidates = Vec::new();
        for (device_index, device) in self.devices.iter().enumerate() {
            for backend in &device.backends {
                let Some(backend_priority) = self.backend_rank(backend.kind) else {
                    continue;
                };
                for mode in &backend.descriptor.modes {
                    let Some(format_priority) = self.format_rank(mode.format.code) else {
                        continue;
                    };
                    let Some(interval) = self.interval_for_mode(mode) else {
                        continue;
                    };
                    let width = mode.format.resolution.width.get();
                    let height = mode.format.resolution.height.get();
                    let Some(resolution_priority) = self.resolution_rank(width, height) else {
                        continue;
                    };
                    let fits_requested_size = self.min_width.is_none_or(|min| width >= min)
                        && self.min_height.is_none_or(|min| height >= min)
                        && self.max_width.is_none_or(|max| width <= max)
                        && self.max_height.is_none_or(|max| height <= max);
                    if !fits_requested_size {
                        continue;
                    }
                    let area = width as u64 * height as u64;
                    let fps_milli = interval.map(interval_fps_milli).unwrap_or(0);
                    let candidate = CameraCandidate {
                        device_index,
                        backend: backend.kind,
                        mode: mode.id.clone(),
                        interval,
                        backend_priority,
                        format_priority,
                        resolution_priority,
                        fps_milli,
                        area,
                    };
                    candidates.push(candidate);
                }
            }
        }
        candidates
    }

    fn backend_rank(&self, backend: BackendKind) -> Option<u8> {
        self.backend_priority
            .iter()
            .position(|candidate| *candidate == backend)
            .map(|index| (self.backend_priority.len().saturating_sub(index)) as u8)
    }

    fn format_rank(&self, code: FourCc) -> Option<u8> {
        self.format_priority
            .iter()
            .position(|candidate| *candidate == code)
            .map(|index| (self.format_priority.len().saturating_sub(index)) as u8)
    }

    fn resolution_rank(&self, width: u32, height: u32) -> Option<u8> {
        if self.resolution_priority.is_empty() {
            return Some(1);
        }
        self.resolution_priority
            .iter()
            .position(|candidate| *candidate == (width, height))
            .map(|index| (self.resolution_priority.len().saturating_sub(index)) as u8)
    }

    fn interval_for_mode(&self, mode: &Mode) -> Option<Option<Interval>> {
        match self.interval_preference {
            CameraIntervalPreference::Default => Some(default_interval(mode)),
            CameraIntervalPreference::Fastest => Some(fastest_interval(mode)),
            CameraIntervalPreference::Slowest => Some(slowest_interval(mode)),
            CameraIntervalPreference::Exact(interval) => {
                if mode.intervals.is_empty() || mode.intervals.contains(&interval) {
                    Some(Some(interval))
                } else {
                    None
                }
            }
            CameraIntervalPreference::None => Some(None),
        }
    }
}

impl Default for CameraRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct SelectedCamera {
    pub device: ProbedDevice,
    pub backend: BackendKind,
    pub mode: ModeId,
    pub interval: Option<Interval>,
    pub controls: Vec<(ControlId, ControlValue)>,
    pub tdn_output_mode: TdnOutputMode,
    pub config: Option<StyxConfig>,
}

impl SelectedCamera {
    pub fn capture_request(&self) -> CaptureRequest<'_> {
        let mut request = CaptureRequest::new(&self.device)
            .backend(self.backend)
            .mode(self.mode.clone())
            .tdn_output_mode(self.tdn_output_mode);
        if let Some(interval) = self.interval {
            request = request.interval(interval);
        }
        for (id, value) in &self.controls {
            request = request.control(*id, value.clone());
        }
        if let Some(config) = &self.config {
            request = request.config(config.clone());
        }
        request
    }

    pub fn start(&self) -> Result<super::super::handle::CaptureHandle, CaptureError> {
        self.capture_request().start()
    }

    pub fn start_with_policy(
        &self,
        policy: CaptureStartPolicy,
    ) -> Result<super::super::handle::CaptureHandle, CaptureError> {
        self.capture_request().start_with_policy(policy)
    }
}

impl AsRef<ProbedDevice> for SelectedCamera {
    fn as_ref(&self) -> &ProbedDevice {
        &self.device
    }
}

impl<'a> From<&'a SelectedCamera> for CaptureRequest<'a> {
    fn from(selected: &'a SelectedCamera) -> Self {
        selected.capture_request()
    }
}

#[derive(Debug, Clone)]
struct CameraCandidate {
    device_index: usize,
    backend: BackendKind,
    mode: ModeId,
    interval: Option<Interval>,
    backend_priority: u8,
    format_priority: u8,
    resolution_priority: u8,
    fps_milli: u64,
    area: u64,
}

impl CameraCandidate {
    fn score(&self) -> (u8, u8, u8, u64, u64) {
        (
            self.format_priority,
            self.resolution_priority,
            self.backend_priority,
            self.fps_milli,
            self.area,
        )
    }
}

fn fastest_interval(mode: &Mode) -> Option<Interval> {
    mode.intervals
        .iter()
        .copied()
        .max_by(|a, b| interval_fps_milli(*a).cmp(&interval_fps_milli(*b)))
        .or_else(|| mode.interval_stepwise.map(|s| s.min))
}

fn slowest_interval(mode: &Mode) -> Option<Interval> {
    mode.intervals
        .iter()
        .copied()
        .min_by(|a, b| interval_fps_milli(*a).cmp(&interval_fps_milli(*b)))
        .or_else(|| mode.interval_stepwise.map(|s| s.max))
}

fn interval_fps_milli(interval: Interval) -> u64 {
    (interval.denominator.get() as u64)
        .saturating_mul(1_000)
        .checked_div(interval.numerator.get().max(1) as u64)
        .unwrap_or(0)
}
