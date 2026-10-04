//! The previous form of a frame request, kept for one release. `styx` converts a
//! [`FrameRequirements`] to its `FrameRequest` (`FrameRequest::from(requirements)`), and every
//! planner function still takes one, so existing code builds with deprecation warnings.
#![allow(deprecated)]

use alloc::{string::String, vec::Vec};

use super::{FrameRect, OutputFormat, PyramidRequest, PyramidSource};
use crate::format::FourCc;

/// What the planner optimised for, and with it the queue depth and the meaning of `min_fps`.
///
/// Deprecated: the planner always takes the cheapest route (CPU and latency together), and the
/// other two meanings have their own choices on `styx::prelude::Frames`: delivery (`latest()`,
/// `every_frame(n)`) and frame rate (`fps(x)` exactly, `fps_at_least(x)`). A `FrameRequirements`
/// converts as: `Latency` → `latest()`, `Throughput` → `every_frame(4)`, `Power` →
/// `every_frame(3)`, and `Power` with `min_fps(x)` → `fps(x)`.
#[deprecated(
    note = "use styx::prelude::Frames: `.latest()` / `.every_frame(n)` for delivery, `.fps(x)` / `.fps_at_least(x)` for the rate"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Priority {
    /// Newest frame only, multi-core decode; `min_fps` takes the camera's fastest rate.
    #[default]
    Latency,
    /// Four queued frames; `min_fps` takes the camera's fastest rate.
    Throughput,
    /// Three queued frames; `min_fps` is the exact rate where the camera can run at any rate.
    Power,
}

/// Whether hardware decode/scaling may be used.
#[deprecated(note = "use styx::planner::Hardware")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum HardwarePolicy {
    /// Use hardware whose feature is enabled and whose runtime probe succeeds.
    #[default]
    Auto,
    /// Software paths only.
    Disabled,
    /// Fail planning unless the decode/scale path runs on hardware.
    Required,
}

/// Explicit choices that take precedence over the planner's own.
#[deprecated(
    note = "use the route methods of styx::prelude::Frames: `.backend(..)`, `.hardware(..)`, `.decoder(..)`, `.forbid(..)`, `.decode_threads(..)`, `.every_frame(n)`"
)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlanOverrides {
    /// Only consider this capture backend (`"libcamera"`, `"v4l2"`, ...).
    pub backend: Option<String>,
    /// Use exactly this decoder implementation (e.g. `"turbojpeg-luma"`, `"ffmpeg-hw"`).
    pub decoder: Option<String>,
    /// Never use these decoder implementations or hardware backends.
    pub forbid: Vec<String>,
    pub hardware: HardwarePolicy,
    /// Decode threads per frame (`None` = derived from [`Priority`]).
    pub decode_threads: Option<usize>,
    /// Frames buffered between capture and consumer (`None` = derived from [`Priority`]).
    pub queue_depth: Option<usize>,
}

/// What one consumer needs from a camera, in the previous form.
#[deprecated(
    note = "use styx::prelude::Frames, e.g. `Frames::nv12().size(1280, 800).fps(30)`; FrameRequest::from(requirements) converts"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FrameRequirements {
    pub output: OutputFormat,
    /// Row stride (and buffer base) alignment in bytes.
    pub stride_alignment: Option<usize>,
    pub pyramid: Option<PyramidRequest>,
    /// Initial region of interest in full-frame coordinates; can be changed while running.
    pub roi: Option<FrameRect>,
    pub min_resolution: Option<(u32, u32)>,
    pub max_resolution: Option<(u32, u32)>,
    /// The frame size the consumer works at.
    pub output_resolution: Option<(u32, u32)>,
    pub min_fps: Option<u32>,
    pub priority: Priority,
    /// Never used by the planner.
    pub strict: bool,
    pub overrides: PlanOverrides,
}

impl FrameRequirements {
    /// 8-bit luma frames.
    pub fn luma() -> Self {
        Self::new(OutputFormat::Luma)
    }

    /// Any of `formats`, most preferred first.
    pub fn formats(formats: impl IntoIterator<Item = FourCc>) -> Self {
        Self::new(OutputFormat::Formats(formats.into_iter().collect()))
    }

    fn new(output: OutputFormat) -> Self {
        Self {
            output,
            stride_alignment: None,
            pyramid: None,
            roi: None,
            min_resolution: None,
            max_resolution: None,
            output_resolution: None,
            min_fps: None,
            priority: Priority::default(),
            strict: false,
            overrides: PlanOverrides::default(),
        }
    }

    pub fn stride_alignment(mut self, bytes: usize) -> Self {
        self.stride_alignment = Some(bytes);
        self
    }

    pub fn pyramid(mut self, levels: u8) -> Self {
        self.pyramid = Some(PyramidRequest {
            levels,
            source: PyramidSource::PreferHardware,
        });
        self
    }

    pub fn pyramid_source(mut self, source: PyramidSource) -> Self {
        if let Some(pyramid) = &mut self.pyramid {
            pyramid.source = source;
        }
        self
    }

    pub fn roi(mut self, roi: FrameRect) -> Self {
        self.roi = Some(roi);
        self
    }

    pub fn min_resolution(mut self, width: u32, height: u32) -> Self {
        self.min_resolution = Some((width, height));
        self
    }

    pub fn max_resolution(mut self, width: u32, height: u32) -> Self {
        self.max_resolution = Some((width, height));
        self
    }

    pub fn output_resolution(mut self, width: u32, height: u32) -> Self {
        self.output_resolution = Some((width, height));
        self
    }

    pub fn min_fps(mut self, fps: u32) -> Self {
        self.min_fps = Some(fps);
        self
    }

    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    pub fn overrides(mut self, overrides: PlanOverrides) -> Self {
        self.overrides = overrides;
        self
    }

    /// Whether `code` satisfies [`FrameRequirements::output`] without conversion.
    pub fn accepts(&self, code: FourCc) -> bool {
        self.output.accepts(code)
    }
}
