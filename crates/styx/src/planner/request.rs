//! What a consumer wants from a camera: [`FrameRequest`], built with [`Frames`].
//!
//! ```no_run
//! use styx::prelude::*;
//!
//! let camera = &styx::probe_all()[0];
//! let mut frames = Frames::nv12() // or rgb(), gray(), formats([..]), any()
//!     .size(1280, 800)            // the nearest the camera does that covers it; never upscaled
//!     .fps(30)                    // exactly 30 fps, or an error naming the rates it can do
//!     .open(camera)?;
//! println!("{}", frames.plan()); // camera, mode, every step and its cost
//! # Ok::<(), styx::planner::OpenError>(())
//! ```
//!
//! Each choice has one meaning:
//!
//! - **Format**: [`Frames::nv12`], [`Frames::rgb`] (RGB24), [`Frames::gray`] (8-bit luma),
//!   [`Frames::formats`] (any of these, most preferred first) or [`Frames::any`] (whatever the
//!   camera delivers).
//! - **Size**: [`FrameRequest::size`] is the size you work at: the camera's smallest mode
//!   covering it, scaled down by the ISP or the JPEG decoder where they can, never upscaled.
//!   [`FrameRequest::size_at_least`] and [`FrameRequest::size_at_most`] bound the capture mode.
//! - **Frame rate** ([`FrameRate`]): none given, the camera's default (30 fps where it can);
//!   [`FrameRequest::fps`] exactly; [`FrameRequest::fps_at_least`] the fastest;
//!   [`FrameRequest::fps_between`] the fastest within a range.
//! - **Delivery** ([`Delivery`]): [`FrameRequest::latest`] (the default) hands out the newest
//!   frame only and drops older ones; [`FrameRequest::every_frame`] queues up to `n` frames and
//!   counts any it still has to drop ([`Frames::dropped`]).
//!
//! Advanced, for consumers with special needs (a feature detector, a SIMD kernel):
//! [`FrameRequest::pyramid`], [`FrameRequest::roi`], [`FrameRequest::row_alignment`]. Route
//! control, when the planner's choice is not wanted: [`FrameRequest::backend`],
//! [`FrameRequest::hardware`], [`FrameRequest::decoder`], [`FrameRequest::forbid`],
//! [`FrameRequest::decode_threads`].

use styx_core::prelude::*;

use super::{FramePlan, Frames, PlanError};
use crate::BackendKind;
use crate::capture_api::CaptureError;
use crate::prelude::ProbedDevice;

/// The frame rate a consumer asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FrameRate {
    /// The camera's default: [`DEFAULT_FPS`](super::DEFAULT_FPS) (30) on a camera that runs at
    /// any rate in a range (a sensor Styx drives), clamped to the range; on a camera with a list
    /// of rates (USB cameras), the listed rate closest to 30.
    #[default]
    CameraDefault,
    /// Exactly this rate: on a camera with a list of rates, a listed rate within 1% of it.
    /// Planning fails, naming the rates there are, when no mode has it.
    Exactly(u32),
    /// The fastest rate of the chosen mode, which must reach this one.
    AtLeast(u32),
    /// The fastest rate within `min..=max` (both inclusive, 1% tolerance on a list of rates).
    Between(u32, u32),
}

/// How frames are handed to the consumer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Delivery {
    /// The newest frame only: a frame not taken before the next one arrives is dropped. The
    /// lowest latency, for consumers that want the present (preview, control loops, detection).
    #[default]
    Latest,
    /// Every frame, queued up to this many while the consumer is busy. Frames still dropped when
    /// the queue is full are counted ([`Frames::dropped`], the health report).
    EveryFrame(usize),
}

impl Delivery {
    /// Frames buffered between capture and consumer.
    pub fn queue_depth(self) -> usize {
        match self {
            Delivery::Latest => 1,
            Delivery::EveryFrame(n) => n.max(1),
        }
    }
}

/// Whether hardware blocks (ISP scaling, hardware decoders and encoders) may be used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Hardware {
    /// Hardware whose feature is enabled and whose runtime probe succeeds.
    #[default]
    Auto,
    /// Fail planning unless the decode or scale path runs on hardware.
    Required,
    /// Software paths only.
    Off,
}

/// What a consumer wants from a camera. Start with [`Frames::nv12`], [`Frames::rgb`],
/// [`Frames::gray`], [`Frames::formats`] or [`Frames::any`]; see the [module](self) for what
/// each choice means. Plain data: it can be stored, compared, sent to a camera service.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct FrameRequest {
    pub format: OutputFormat,
    /// The size the consumer works at ([`FrameRequest::size`]).
    pub size: Option<(u32, u32)>,
    /// Smallest capture mode allowed.
    pub min_size: Option<(u32, u32)>,
    /// Largest capture mode allowed.
    pub max_size: Option<(u32, u32)>,
    pub fps: FrameRate,
    pub delivery: Delivery,
    /// Advanced: pyramid companions.
    pub pyramid: Option<PyramidRequest>,
    /// Advanced: initial region of interest in full-frame pixels; can change while running.
    pub roi: Option<FrameRect>,
    /// Advanced: row stride (and buffer base) alignment in bytes.
    pub row_alignment: Option<usize>,
    /// Route control: only this capture backend.
    pub backend: Option<BackendKind>,
    pub hardware: Hardware,
    /// Route control: exactly this decoder implementation (e.g. `"turbojpeg-luma"`).
    pub decoder: Option<String>,
    /// Route control: never these decoder or encoder implementations.
    pub forbid: Vec<String>,
    /// Route control: decode threads per frame (0 = automatic). Otherwise automatic with
    /// [`Delivery::Latest`] (several cores per frame when the JPEG allows), one with
    /// [`Delivery::EveryFrame`].
    pub decode_threads: Option<usize>,
    /// Fail rather than deliver frames that do not meet the request ([`FrameRequest::strict`]).
    pub strict: bool,
}

impl Default for FrameRequest {
    fn default() -> Self {
        Self::new(OutputFormat::Any)
    }
}

impl Frames {
    /// NV12 frames (8-bit 4:2:0, Y plane then interleaved UV).
    pub fn nv12() -> FrameRequest {
        FrameRequest::formats([FourCc::NV12])
    }

    /// RGB24 frames.
    pub fn rgb() -> FrameRequest {
        FrameRequest::formats([FourCc::RG24])
    }

    /// 8-bit luma (grey) frames; the Y plane of NV12 or YUV frames without copying.
    pub fn gray() -> FrameRequest {
        FrameRequest::new(OutputFormat::Luma)
    }

    /// Any of `formats`, most preferred first (compressed ones, e.g. H.264, are encoded when the
    /// camera does not produce them).
    pub fn formats(formats: impl IntoIterator<Item = FourCc>) -> FrameRequest {
        FrameRequest::formats(formats)
    }

    /// Whatever the camera delivers, unconverted.
    pub fn any() -> FrameRequest {
        FrameRequest::default()
    }
}

impl FrameRequest {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            size: None,
            min_size: None,
            max_size: None,
            fps: FrameRate::CameraDefault,
            delivery: Delivery::Latest,
            pyramid: None,
            roi: None,
            row_alignment: None,
            backend: None,
            hardware: Hardware::Auto,
            decoder: None,
            forbid: Vec::new(),
            decode_threads: None,
            strict: false,
        }
    }

    /// Any of `formats`, most preferred first.
    pub fn formats(formats: impl IntoIterator<Item = FourCc>) -> Self {
        Self::new(OutputFormat::Formats(formats.into_iter().collect()))
    }

    /// The size you work at: the camera's smallest mode covering it, scaled down to it by the
    /// ISP (keeping the mode's aspect ratio) or the JPEG decoder (½, ¼, ⅛) where they can.
    /// Frames are never upscaled; [`FramePlan::output_resolution`] says what arrives.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.size = Some((width, height));
        self
    }

    /// Only capture modes at least this large (the smallest such mode is taken).
    pub fn size_at_least(mut self, width: u32, height: u32) -> Self {
        self.min_size = Some((width, height));
        self
    }

    /// Only capture modes at most this large.
    pub fn size_at_most(mut self, width: u32, height: u32) -> Self {
        self.max_size = Some((width, height));
        self
    }

    /// Exactly `fps` frames per second ([`FrameRate::Exactly`]).
    pub fn fps(mut self, fps: u32) -> Self {
        self.fps = FrameRate::Exactly(fps);
        self
    }

    /// At least `fps`: the camera's fastest rate ([`FrameRate::AtLeast`]).
    pub fn fps_at_least(mut self, fps: u32) -> Self {
        self.fps = FrameRate::AtLeast(fps);
        self
    }

    /// The fastest rate from `min` to `max` fps ([`FrameRate::Between`]).
    pub fn fps_between(mut self, min: u32, max: u32) -> Self {
        self.fps = FrameRate::Between(min.min(max), max.max(min));
        self
    }

    /// The newest frame only, older ones dropped ([`Delivery::Latest`], the default).
    pub fn latest(mut self) -> Self {
        self.delivery = Delivery::Latest;
        self
    }

    /// Every frame, queued up to `n` ([`Delivery::EveryFrame`]).
    pub fn every_frame(mut self, n: usize) -> Self {
        self.delivery = Delivery::EveryFrame(n.max(1));
        self
    }

    /// Advanced: attach `levels` pyramid companions (½, ¼, ...) to each frame, the first from
    /// the ISP where it can. Uncompressed frames only.
    pub fn pyramid(mut self, levels: u8) -> Self {
        self.pyramid = Some(PyramidRequest {
            levels,
            source: PyramidSource::PreferHardware,
        });
        self
    }

    /// Advanced: where pyramid levels come from (after [`FrameRequest::pyramid`]).
    pub fn pyramid_source(mut self, source: PyramidSource) -> Self {
        if let Some(pyramid) = &mut self.pyramid {
            pyramid.source = source;
        }
        self
    }

    /// Advanced: deliver only this region (full-frame pixels; luma frames), changeable while
    /// running ([`Frames::roi`]). An MJPEG decode skips the rows below it.
    pub fn roi(mut self, roi: FrameRect) -> Self {
        self.roi = Some(roi);
        self
    }

    /// Advanced: rows (and the buffer start) aligned to `bytes`, e.g. 64 for SIMD loads; rows
    /// are copied only where the camera's are not aligned already.
    pub fn row_alignment(mut self, bytes: usize) -> Self {
        self.row_alignment = Some(bytes);
        self
    }

    /// Route control: only this capture backend.
    pub fn backend(mut self, backend: BackendKind) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Route control: whether hardware blocks may be used ([`Hardware::Auto`] by default).
    pub fn hardware(mut self, hardware: Hardware) -> Self {
        self.hardware = hardware;
        self
    }

    /// Route control: decode with exactly this implementation (e.g. `"turbojpeg-luma"`).
    pub fn decoder(mut self, name: impl Into<String>) -> Self {
        self.decoder = Some(name.into());
        self
    }

    /// Route control: never use this decoder or encoder implementation.
    pub fn forbid(mut self, name: impl Into<String>) -> Self {
        self.forbid.push(name.into());
        self
    }

    /// Route control: decode threads per frame (0 = automatic).
    pub fn decode_threads(mut self, threads: usize) -> Self {
        self.decode_threads = Some(threads);
        self
    }

    /// Fail planning instead of delivering frames that miss part of the request (an
    /// [`Unmet`](super::Unmet), e.g. a size no route scales to). By default the plan delivers the
    /// nearest it can and says what it missed ([`FramePlan::delivered`]).
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    /// Whether `code` is the format asked for without conversion.
    pub fn accepts(&self, code: FourCc) -> bool {
        self.format.accepts(code)
    }

    /// Decode threads per frame: the explicit choice, else from the delivery.
    pub(crate) fn threads(&self) -> usize {
        self.decode_threads.unwrap_or(match self.delivery {
            // 0 = automatic (up to four cores when the JPEG has restart markers).
            Delivery::Latest => 0,
            Delivery::EveryFrame(_) => 1,
        })
    }

    /// The plan for these frames from `camera`, without starting it (print it, adjust it, then
    /// [`FramePlan::start`]).
    pub fn plan(&self, camera: &ProbedDevice) -> Result<FramePlan, PlanError> {
        super::plan_frames(camera, self)
    }

    /// The plan from the best of `cameras` (see [`FrameRequest::plan`]).
    pub fn plan_best(&self, cameras: &[ProbedDevice]) -> Result<FramePlan, PlanError> {
        super::plan_best(cameras, self)
    }

    /// Plan these frames from `camera` and start capturing.
    pub fn open(&self, camera: &ProbedDevice) -> Result<Frames, OpenError> {
        Ok(self.plan(camera)?.start()?)
    }

    /// Plan these frames from the best of `cameras` and start capturing.
    pub fn open_best(&self, cameras: &[ProbedDevice]) -> Result<Frames, OpenError> {
        Ok(self.plan_best(cameras)?.start()?)
    }
}

/// Why frames could not be opened: no route meets the request, or the camera did not start.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Capture(#[from] CaptureError),
}

/// A [`FrameRequest`] for one camera, from [`ProbedDevice::frames`]:
/// `camera.frames().nv12().size(1280, 800).fps(30).open()`.
#[derive(Debug, Clone)]
pub struct CameraFrames<'a> {
    camera: &'a ProbedDevice,
    request: FrameRequest,
}

macro_rules! forward {
    ($($(#[$doc:meta])* $name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            $(#[$doc])*
            pub fn $name(mut self, $($arg: $ty),*) -> Self {
                self.request = self.request.$name($($arg),*);
                self
            }
        )*
    };
}

impl<'a> CameraFrames<'a> {
    /// NV12 frames ([`Frames::nv12`]).
    pub fn nv12(mut self) -> Self {
        self.request.format = Frames::nv12().format;
        self
    }

    /// RGB24 frames ([`Frames::rgb`]).
    pub fn rgb(mut self) -> Self {
        self.request.format = Frames::rgb().format;
        self
    }

    /// 8-bit luma frames ([`Frames::gray`]).
    pub fn gray(mut self) -> Self {
        self.request.format = OutputFormat::Luma;
        self
    }

    /// Any of `formats`, most preferred first ([`Frames::formats`]).
    pub fn formats(mut self, formats: impl IntoIterator<Item = FourCc>) -> Self {
        self.request.format = OutputFormat::Formats(formats.into_iter().collect());
        self
    }

    /// Whatever the camera delivers (the default here; [`Frames::any`]).
    pub fn any(mut self) -> Self {
        self.request.format = OutputFormat::Any;
        self
    }

    forward! {
        /// [`FrameRequest::size`].
        size(width: u32, height: u32);
        /// [`FrameRequest::size_at_least`].
        size_at_least(width: u32, height: u32);
        /// [`FrameRequest::size_at_most`].
        size_at_most(width: u32, height: u32);
        /// [`FrameRequest::fps`].
        fps(fps: u32);
        /// [`FrameRequest::fps_at_least`].
        fps_at_least(fps: u32);
        /// [`FrameRequest::fps_between`].
        fps_between(min: u32, max: u32);
        /// [`FrameRequest::latest`].
        latest();
        /// [`FrameRequest::every_frame`].
        every_frame(n: usize);
        /// [`FrameRequest::pyramid`].
        pyramid(levels: u8);
        /// [`FrameRequest::pyramid_source`].
        pyramid_source(source: PyramidSource);
        /// [`FrameRequest::roi`].
        roi(roi: FrameRect);
        /// [`FrameRequest::row_alignment`].
        row_alignment(bytes: usize);
        /// [`FrameRequest::backend`].
        backend(backend: BackendKind);
        /// [`FrameRequest::hardware`].
        hardware(hardware: Hardware);
        /// [`FrameRequest::decode_threads`].
        decode_threads(threads: usize);
        /// [`FrameRequest::strict`].
        strict();
    }

    /// [`FrameRequest::decoder`].
    pub fn decoder(mut self, name: impl Into<String>) -> Self {
        self.request = self.request.decoder(name);
        self
    }

    /// [`FrameRequest::forbid`].
    pub fn forbid(mut self, name: impl Into<String>) -> Self {
        self.request = self.request.forbid(name);
        self
    }

    /// The request built so far.
    pub fn request(&self) -> &FrameRequest {
        &self.request
    }

    /// The plan, without starting it ([`FrameRequest::plan`]).
    pub fn plan(&self) -> Result<FramePlan, PlanError> {
        self.request.plan(self.camera)
    }

    /// Plan and start capturing ([`FrameRequest::open`]).
    pub fn open(&self) -> Result<Frames, OpenError> {
        self.request.open(self.camera)
    }
}

impl ProbedDevice {
    /// Frames from this camera: `camera.frames().nv12().size(1280, 800).fps(30).open()`. The
    /// same choices as [`Frames`]; the format is the camera's own until one is chosen.
    pub fn frames(&self) -> CameraFrames<'_> {
        CameraFrames {
            camera: self,
            request: FrameRequest::default(),
        }
    }
}

impl From<CameraFrames<'_>> for FrameRequest {
    fn from(frames: CameraFrames<'_>) -> Self {
        frames.request
    }
}

#[allow(deprecated)]
mod legacy {
    use styx_core::prelude::{FrameRequirements, HardwarePolicy, Priority};

    use super::{Delivery, FrameRate, FrameRequest, Hardware};

    /// The previous request's meaning, kept: `Priority::Power` with `min_fps` was an exact rate
    /// (on cameras that run at any rate in a range), otherwise `min_fps` took the fastest rate;
    /// `Latency` delivered the newest frame, `Throughput` and `Power` queued 4 and 3 frames
    /// with one decode thread each. The priority's route scoring is gone: one score for all.
    impl From<&FrameRequirements> for FrameRequest {
        fn from(old: &FrameRequirements) -> Self {
            let o = &old.overrides;
            let delivery = match (o.queue_depth, old.priority) {
                (Some(n), _) if n > 1 => Delivery::EveryFrame(n),
                (Some(_), _) | (None, Priority::Latency) => Delivery::Latest,
                (None, Priority::Throughput) => Delivery::EveryFrame(4),
                (None, Priority::Power) => Delivery::EveryFrame(3),
            };
            let fps = match (old.min_fps, old.priority) {
                (None, _) => FrameRate::CameraDefault,
                (Some(fps), Priority::Power) => FrameRate::Exactly(fps),
                (Some(fps), _) => FrameRate::AtLeast(fps),
            };
            let backend = o.backend.as_deref().and_then(|name| {
                name.parse()
                    .inspect_err(|err| tracing::warn!(%err, "backend override ignored"))
                    .ok()
            });
            let mut new = FrameRequest {
                format: old.output.clone(),
                size: old.output_resolution,
                min_size: old.min_resolution,
                max_size: old.max_resolution,
                fps,
                delivery,
                pyramid: old.pyramid,
                roi: old.roi,
                row_alignment: old.stride_alignment,
                backend,
                hardware: match o.hardware {
                    HardwarePolicy::Auto => Hardware::Auto,
                    HardwarePolicy::Required => Hardware::Required,
                    HardwarePolicy::Disabled => Hardware::Off,
                },
                decoder: o.decoder.clone(),
                forbid: o.forbid.clone(),
                decode_threads: o.decode_threads,
                strict: old.strict,
            };
            // The priority set the threads (automatic for latency), not the queue depth: keep
            // them where the delivery would now choose others.
            let threads = match old.priority {
                Priority::Latency => 0,
                Priority::Throughput | Priority::Power => 1,
            };
            if new.threads() != threads {
                new.decode_threads = Some(threads);
            }
            new
        }
    }

    impl From<FrameRequirements> for FrameRequest {
        fn from(old: FrameRequirements) -> Self {
            FrameRequest::from(&old)
        }
    }
}
