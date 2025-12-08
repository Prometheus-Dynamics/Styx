#![doc = include_str!("../README.md")]

use smallvec::SmallVec;

use styx_core::prelude::*;

/// Identifier for a capture mode keyed by its format and optional interval.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let res = Resolution::new(640, 480).unwrap();
/// let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
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
/// let res = Resolution::new(320, 240).unwrap();
/// let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
/// let mode = Mode {
///     id: ModeId { format, interval: None },
///     format,
///     intervals: smallvec::smallvec![],
///     interval_stepwise: None,
/// };
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

/// Descriptor for a capture device/source.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let res = Resolution::new(320, 240).unwrap();
/// let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
/// let mode = Mode {
///     id: ModeId { format, interval: None },
///     format,
///     intervals: smallvec::smallvec![],
///     interval_stepwise: None,
/// };
/// let descriptor = CaptureDescriptor { modes: vec![mode], controls: Vec::new() };
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

/// User-selected configuration validated against a descriptor.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let res = Resolution::new(320, 240).unwrap();
/// let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
/// let mode = Mode {
///     id: ModeId { format, interval: None },
///     format,
///     intervals: smallvec::smallvec![],
///     interval_stepwise: None,
/// };
/// let descriptor = CaptureDescriptor { modes: vec![mode.clone()], controls: Vec::new() };
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
/// ```rust,ignore
/// use styx_capture::prelude::*;
///
/// struct MySource;
/// impl CaptureSource for MySource {
///     fn descriptor(&self) -> &CaptureDescriptor { unimplemented!() }
///     fn next_frame(&self) -> Option<FrameLease> { None }
/// }
/// ```
pub trait CaptureSource: Send + Sync {
    /// Descriptor for this source.
    fn descriptor(&self) -> &CaptureDescriptor;

    /// Pull the next frame; concrete backends decide how to block/yield.
    fn next_frame(&self) -> Option<FrameLease>;
}

/// Helper to construct a simple frame from a pooled buffer.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let pool = BufferPool::with_capacity(1, 64);
/// let res = Resolution::new(2, 2).unwrap();
/// let format = MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb);
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
    let meta = FrameMeta::new(format, timestamp);
    FrameLease::single_plane(meta, pool.lease(), layout.len, layout.stride)
}

/// Utility to create a mode id list from formats.
///
/// # Example
/// ```rust
/// use styx_capture::prelude::*;
///
/// let res = Resolution::new(2, 2).unwrap();
/// let formats = [MediaFormat::new(FourCc::new(*b"RG24"), res, ColorSpace::Srgb)];
/// let modes = modes_from_formats(formats);
/// assert_eq!(modes.len(), 1);
/// ```
pub fn modes_from_formats(formats: impl IntoIterator<Item = MediaFormat>) -> Vec<Mode> {
    formats
        .into_iter()
        .map(|format| Mode {
            id: ModeId {
                format,
                interval: None,
            },
            format,
            intervals: SmallVec::new(),
            interval_stepwise: None,
        })
        .collect()
}

pub mod virtual_backend;

pub mod prelude {
    pub use crate::{
        CaptureConfig, CaptureDescriptor, CaptureSource, Mode, ModeId, build_frame_from_pool,
        modes_from_formats, virtual_backend::VirtualCapture,
    };
    pub use styx_core::prelude::*;
}
