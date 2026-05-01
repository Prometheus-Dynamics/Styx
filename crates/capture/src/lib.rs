#![doc = include_str!("../README.md")]
#![deny(clippy::print_stderr, clippy::print_stdout)]

use smallvec::SmallVec;

use styx_core::prelude::*;

/// Identifier for a capture mode keyed by its format and optional interval.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let res = Resolution::new(640, 480).unwrap();
/// let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
/// let id = ModeId { format, interval: None };
/// assert_eq!(id.format.code.to_string(), "RG24");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ModeId {
    /// Pixel format and resolution for this mode.
    pub format: MediaFormat,
    /// Optional interval associated with this mode (if the mode is interval-specific).
    pub interval: Option<Interval>,
}

/// Descriptor for a single capture mode (format + intervals).
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let format = MediaFormat::srgb(FourCc::RG24, 320, 240).unwrap();
/// let mode = Mode::new(format);
/// assert_eq!(mode.format.code.to_string(), "RG24");
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct Mode {
    /// Identifier (format + optional interval) for this mode.
    pub id: ModeId,
    /// Media format associated with the mode.
    pub format: MediaFormat,
    /// Supported frame intervals.
    #[cfg_attr(feature = "schema", schema(value_type = Vec<Interval>))]
    pub intervals: SmallVec<[Interval; 4]>,
    /// Optional stepwise interval range.
    #[cfg_attr(feature = "schema", schema(value_type = Option<IntervalStepwise>))]
    pub interval_stepwise: Option<IntervalStepwise>,
}

impl Mode {
    /// Create a mode for a format without advertised frame intervals.
    pub fn new(format: MediaFormat) -> Self {
        Self {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: SmallVec::new(),
            interval_stepwise: None,
        }
    }

    /// Create a mode for a format with one advertised frame interval.
    pub fn with_interval(format: MediaFormat, interval: Interval) -> Self {
        Self {
            id: ModeId {
                format,
                interval: Some(interval),
            },
            format,
            intervals: smallvec::smallvec![interval],
            interval_stepwise: None,
        }
    }
}

/// Descriptor for a capture device/source.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let format = MediaFormat::srgb(FourCc::RG24, 320, 240).unwrap();
/// let descriptor = CaptureDescriptor::new([Mode::new(format)]);
/// assert_eq!(descriptor.modes.len(), 1);
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct CaptureDescriptor {
    /// Supported modes.
    pub modes: Vec<Mode>,
    /// Supported controls.
    pub controls: Vec<ControlMeta>,
}

impl CaptureDescriptor {
    /// Build a descriptor with no controls.
    pub fn new(modes: impl IntoIterator<Item = Mode>) -> Self {
        Self {
            modes: modes.into_iter().collect(),
            controls: Vec::new(),
        }
    }

    /// Attach controls to a descriptor.
    pub fn with_controls(mut self, controls: Vec<ControlMeta>) -> Self {
        self.controls = controls;
        self
    }
}

/// User-selected configuration validated against a descriptor.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let format = MediaFormat::srgb(FourCc::RG24, 320, 240).unwrap();
/// let mode = Mode::new(format);
/// let descriptor = CaptureDescriptor::new([mode.clone()]);
/// let cfg = CaptureConfig { mode: mode.id.clone(), interval: None, controls: vec![] };
/// assert!(cfg.validate(&descriptor).is_ok());
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct CaptureConfig {
    /// Selected mode.
    pub mode: ModeId,
    /// Optional interval override.
    pub interval: Option<Interval>,
    /// Control assignments.
    pub controls: Vec<(ControlId, ControlValue)>,
}

impl CaptureConfig {
    /// Validate a config against a descriptor.
    pub fn validate(&self, descriptor: &CaptureDescriptor) -> Result<(), String> {
        let mode = descriptor
            .modes
            .iter()
            .find(|m| m.id.format == self.mode.format)
            .ok_or_else(|| "mode not found".to_string())?;

        let interval = self.mode.interval.or(self.interval);
        if let Some(interval) = &interval {
            // Some backends (notably libcamera) may not advertise frame interval data even though
            // they can accept an interval request via controls. If the mode provides no interval
            // metadata at all, allow any interval through validation.
            let has_interval_metadata =
                !mode.intervals.is_empty() || mode.interval_stepwise.is_some();
            if has_interval_metadata {
                let supported = mode.intervals.iter().any(|iv| iv == interval)
                    || mode
                        .interval_stepwise
                        .as_ref()
                        .map(|sw| sw.contains(*interval))
                        .unwrap_or(false);
                if !supported {
                    return Err("interval not supported by mode".into());
                }
            }
        }

        for (id, value) in &self.controls {
            let Some(meta) = descriptor.controls.iter().find(|c| c.id == *id) else {
                return Err(format!("control {:?} not supported by descriptor", id));
            };
            if matches!(meta.access, Access::ReadOnly) {
                return Err(format!("control {} is read-only", meta.name));
            }
            if !meta.validate(value) {
                return Err(format!("control {} rejected value", meta.name));
            }
        }

        Ok(())
    }
}

/// Trait implemented by capture backends that yield zero-copy frames.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// struct MySource {
///     descriptor: CaptureDescriptor,
/// }
///
/// impl CaptureSource for MySource {
///     fn descriptor(&self) -> &CaptureDescriptor { &self.descriptor }
///     fn next_frame(&self) -> Option<FrameLease> { None }
/// }
///
/// let format = MediaFormat::srgb(FourCc::RG24, 320, 180).unwrap();
/// let source = MySource {
///     descriptor: CaptureDescriptor::new([Mode::new(format)]),
/// };
/// assert_eq!(source.descriptor().modes.len(), 1);
/// ```
pub trait CaptureSource: Send + Sync {
    /// Descriptor for this source.
    fn descriptor(&self) -> &CaptureDescriptor;

    /// Pull the next frame; concrete backends decide how to block/yield.
    fn next_frame(&self) -> Option<FrameLease>;

    /// Pull the next frame with explicit queue-style semantics.
    ///
    /// The default adapts legacy `next_frame` implementations by treating `None`
    /// as closure. Backends with nonblocking or timeout behavior can override this
    /// to return `RecvOutcome::Empty` when no frame is currently ready.
    fn try_next_frame(&self) -> RecvOutcome<FrameLease> {
        match self.next_frame() {
            Some(frame) => RecvOutcome::Data(frame),
            None => RecvOutcome::Closed,
        }
    }
}

/// Helper to construct a simple frame from a pooled buffer.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let pool = BufferPool::with_capacity(1, 64);
/// let res = Resolution::new(2, 2).unwrap();
/// let format = MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb);
/// let frame = build_frame_from_pool(format, &pool, 0, 3);
/// assert_eq!(frame.meta().format.code.to_string(), "RG24");
/// ```
pub fn build_frame_from_pool(
    format: MediaFormat,
    pool: &BufferPool,
    timestamp: u64,
    bytes_per_pixel: usize,
) -> FrameLease {
    let layout = plane_layout_from_dims(
        format.resolution.width,
        format.resolution.height,
        bytes_per_pixel,
    );
    let meta = FrameMeta::new(format, timestamp)
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostOwned,
            to: FrameResidency::HostOwned,
            reason: ResidencyTransitionReason::Capture,
            copied: false,
        });
    FrameLease::single_plane(meta, pool.lease(), layout.len, layout.stride)
}

#[cfg(target_os = "linux")]
pub fn build_frame_from_shared_pool(
    format: MediaFormat,
    pool: &SharedBufferPool,
    timestamp: u64,
    bytes_per_pixel: usize,
) -> Result<FrameLease, FrameExportError> {
    let layout = plane_layout_from_dims(
        format.resolution.width,
        format.resolution.height,
        bytes_per_pixel,
    );
    let meta = FrameMeta::new(format, timestamp)
        .with_capture_instant(std::time::Instant::now())
        .with_transition(ResidencyTransition {
            from: FrameResidency::HostExternal,
            to: FrameResidency::HostExternal,
            reason: ResidencyTransitionReason::Capture,
            copied: false,
        });
    FrameLease::single_plane_shared(meta, pool.lease()?, layout.len, layout.stride)
}

/// Utility to create a mode id list from formats.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let res = Resolution::new(2, 2).unwrap();
/// let formats = [MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb)];
/// let modes = modes_from_formats(formats);
/// assert_eq!(modes.len(), 1);
/// ```
pub fn modes_from_formats(formats: impl IntoIterator<Item = MediaFormat>) -> Vec<Mode> {
    formats.into_iter().map(Mode::new).collect()
}

pub mod virtual_backend;

pub mod prelude {
    #[cfg(target_os = "linux")]
    pub use crate::build_frame_from_shared_pool;
    pub use crate::{
        CaptureConfig, CaptureDescriptor, CaptureSource, Mode, ModeId, build_frame_from_pool,
        modes_from_formats, virtual_backend::VirtualCapture,
    };
    pub use styx_core::prelude::*;
}
