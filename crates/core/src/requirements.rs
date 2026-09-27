//! What a frame consumer needs, independent of the camera or codec that will provide it.
//!
//! A consumer (e.g. a fiducial detector) states the frames it wants: Y8 rows aligned to 64
//! bytes, a ½ and ¼ pyramid, a region of interest, low latency. Styx's planner (in the `styx`
//! crate) turns that into a concrete capture + decode + preparation plan for a device, using
//! hardware only where the enabled features and a runtime probe say it is available.
//!
//! Planning is single-consumer today. Several consumers sharing one camera (e.g. a detector
//! wanting Y8 and a recorder wanting MJPEG) will be planned as one capture with per-consumer
//! branches; the types here are per-consumer so that extension does not change them.

use crate::format::FourCc;

/// A rectangle in full-frame pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct FrameRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl FrameRect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// This rectangle clipped to a `width` x `height` frame; `None` if nothing is left.
    pub fn clipped_to(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let right = self.x.saturating_add(self.width).min(width);
        let bottom = self.y.saturating_add(self.height).min(height);
        (right > x && bottom > y).then_some(Self::new(x, y, right - x, bottom - y))
    }

    /// The same region at `2^-level` scale (pyramid companion coordinates), rounded outward.
    pub fn scaled_down(self, level: u8) -> Self {
        let shift = u32::from(level.min(31));
        let x = self.x >> shift;
        let y = self.y >> shift;
        let right = (self.x + self.width).div_ceil(1 << shift);
        let bottom = (self.y + self.height).div_ceil(1 << shift);
        Self::new(x, y, right - x, bottom - y)
    }

    /// This region of a `from`-sized frame in a `to`-sized view of the same frame, rounded
    /// outward so it covers every pixel of the original.
    pub fn scaled(self, from: (u32, u32), to: (u32, u32)) -> Self {
        let (fw, fh) = (u64::from(from.0.max(1)), u64::from(from.1.max(1)));
        let (tw, th) = (u64::from(to.0), u64::from(to.1));
        let x = (u64::from(self.x) * tw / fw) as u32;
        let y = (u64::from(self.y) * th / fh) as u32;
        let right = (u64::from(self.x + self.width) * tw).div_ceil(fw) as u32;
        let bottom = (u64::from(self.y + self.height) * th).div_ceil(fh) as u32;
        Self::new(x, y, right - x, bottom - y)
    }
}

/// Pixel data the consumer wants.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OutputFormat {
    /// 8-bit luma (GREY). Zero-copy Y-plane views of NV12/YUV420 count.
    Luma,
    /// Any of these formats, most preferred first.
    Formats(Vec<FourCc>),
}

/// Where pyramid levels may come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PyramidSource {
    /// An ISP/scaler output when available, else CPU box filters.
    #[default]
    PreferHardware,
    /// Fail planning unless hardware produces every level.
    HardwareOnly,
    /// Always CPU box filters.
    Software,
}

/// Pyramid companions to attach (`levels` = 2 means ½ and ¼).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PyramidRequest {
    pub levels: u8,
    pub source: PyramidSource,
}

/// What the planner optimises for when several plans satisfy the requirements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Priority {
    /// Shortest time from sensor to consumer: fewer queued frames, multi-core decode.
    #[default]
    Latency,
    /// Most frames per second for the least total CPU time.
    Throughput,
    /// Least CPU: prefer hardware blocks even when they add latency.
    Power,
}

/// Whether hardware decode/scaling may be used.
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

/// What one consumer needs from a camera. Build with [`FrameRequirements::luma`] or
/// [`FrameRequirements::formats`] and the builder methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FrameRequirements {
    pub output: OutputFormat,
    /// Row stride (and buffer base) alignment in bytes, e.g. 64 for SIMD loads.
    pub stride_alignment: Option<usize>,
    pub pyramid: Option<PyramidRequest>,
    /// Initial region of interest in full-frame coordinates; can be changed while running.
    pub roi: Option<FrameRect>,
    pub min_resolution: Option<(u32, u32)>,
    pub max_resolution: Option<(u32, u32)>,
    /// The frame size the consumer works at; see [`FrameRequirements::output_resolution`].
    pub output_resolution: Option<(u32, u32)>,
    pub min_fps: Option<u32>,
    pub priority: Priority,
    /// Fail instead of falling back when a requirement cannot be met exactly.
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

    /// Attach `levels` pyramid companions (½, ¼, ...), hardware-produced when possible.
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

    /// The frame size the consumer works at, e.g. 320x180 for a detector that downsizes
    /// anyway. The planner prefers the smallest capture mode that covers it, and MJPEG decoded
    /// with turbojpeg is decoded straight to ½, ¼ or ⅛ size (the smallest that still covers
    /// it), which costs less CPU and memory than decoding in full. Frames are never upscaled;
    /// routes that cannot scale deliver the capture size (`FramePlan::output_resolution` in
    /// `styx` tells which).
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
        match &self.output {
            OutputFormat::Luma => matches!(code, FourCc::GREY | FourCc::R8),
            OutputFormat::Formats(formats) => formats.contains(&code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_clipping_and_pyramid_scaling() {
        let rect = FrameRect::new(100, 50, 300, 200);
        assert_eq!(
            rect.clipped_to(320, 240),
            Some(FrameRect::new(100, 50, 220, 190))
        );
        assert_eq!(rect.clipped_to(50, 50), None);
        assert_eq!(rect.scaled_down(1), FrameRect::new(50, 25, 150, 100));
        assert_eq!(
            FrameRect::new(3, 3, 2, 2).scaled_down(1),
            FrameRect::new(1, 1, 2, 2)
        );
    }

    #[test]
    fn builder_sets_luma_pyramid_and_roi() {
        let req = FrameRequirements::luma()
            .stride_alignment(64)
            .pyramid(2)
            .pyramid_source(PyramidSource::Software)
            .roi(FrameRect::new(0, 0, 64, 64))
            .priority(Priority::Throughput);
        assert!(req.accepts(FourCc::GREY));
        assert!(!req.accepts(FourCc::NV12));
        assert_eq!(req.pyramid.unwrap().source, PyramidSource::Software);
        assert_eq!(req.priority, Priority::Throughput);
    }
}
