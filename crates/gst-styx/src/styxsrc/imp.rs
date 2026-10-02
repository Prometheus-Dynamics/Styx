use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;
use styx::prelude::*;

use crate::buffer::{Backing, BufferError, MemoryKind, frame_buffer};
use crate::caps::{self, Negotiated};
use crate::source::{self, Stream, StreamOptions};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "styxsrc",
        gst::DebugColorFlags::empty(),
        Some("Styx camera source"),
    )
});

/// How long one wait for a frame lasts before checking for flushing.
const POLL: Duration = Duration::from_millis(50);
const DEFAULT_TIMEOUT_MS: u32 = 5000;

#[derive(Debug, Clone)]
struct Settings {
    camera: Option<String>,
    service: Option<String>,
    extra_controls: Option<gst::Structure>,
    priority: String,
    queue_depth: u32,
    timeout_ms: u32,
    export_dmabuf: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            camera: None,
            service: None,
            extra_controls: None,
            priority: "latency".into(),
            queue_depth: 0,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            export_dmabuf: false,
        }
    }
}

impl Settings {
    fn priority(&self) -> Priority {
        match self.priority.as_str() {
            "throughput" => Priority::Throughput,
            "power" => Priority::Power,
            _ => Priority::Latency,
        }
    }
}

#[derive(Default)]
struct State {
    /// The camera (in-process mode), chosen in `start`.
    device: Option<ProbedDevice>,
    /// What the camera can produce, computed once per start.
    caps: Option<gst::Caps>,
    stream: Option<Stream>,
    negotiated: Option<Negotiated>,
    plan: Option<String>,
    frames: u64,
    last_frame: Option<Instant>,
    /// Exporting dma-bufs failed once; wrap memory from then on (`export-dmabuf` only).
    export_failed: bool,
    /// dma-buf caps were negotiated but the camera's buffers cannot be exported.
    dmabuf_unavailable: bool,
}

#[derive(Default)]
pub struct StyxSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    flushing: AtomicBool,
}

impl StyxSrc {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn settings(&self) -> Settings {
        self.settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn apply_controls(&self, state: &mut State, controls: &gst::StructureRef) {
        let (Some(device), Some(stream)) = (state.device.as_ref(), state.stream.as_mut()) else {
            return;
        };
        for control in source::resolve_controls(device, controls) {
            let result = control.and_then(|(id, value)| stream.set_control(id, value));
            if let Err(err) = result {
                gst::warning!(CAT, imp = self, "{err}");
                gst::element_imp_warning!(self, gst::LibraryError::Settings, ("{err}"));
            }
        }
    }

    /// Choose the camera and work out its caps (in-process mode). Done when going to READY so
    /// caps queries see the camera's formats, as with v4l2src.
    fn open_camera(&self, state: &mut State) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings();
        if settings.service.is_some() {
            return Ok(()); // the service has the camera; caps come from the template
        }
        let device = source::find_camera(settings.camera.as_deref())
            .map_err(|err| gst::error_msg!(gst::ResourceError::NotFound, ("{err}")))?;
        gst::info!(CAT, imp = self, "camera {}", device.identity.display);
        let caps = caps::device_caps(&device);
        if caps.is_empty() {
            return Err(gst::error_msg!(
                gst::ResourceError::Settings,
                (
                    "camera {} has no formats GStreamer can use",
                    device.identity.display
                )
            ));
        }
        gst::debug!(CAT, imp = self, "camera caps {caps}");
        state.caps = Some(caps);
        state.device = Some(device);
        Ok(())
    }

    fn renegotiate_without_dmabuf(&self) -> Result<(), gst::FlowError> {
        if !self.obj().negotiate() {
            gst::element_imp_error!(
                self,
                gst::CoreError::Negotiation,
                ("downstream wants dma-bufs but this camera's buffers cannot be exported"),
                ["Negotiate system memory caps (e.g. put videoconvert after styxsrc)"]
            );
            return Err(gst::FlowError::NotNegotiated);
        }
        Ok(())
    }

    fn stop_stream(state: &mut State) {
        if let Some(stream) = state.stream.take() {
            stream.stop();
        }
        state.negotiated = None;
    }

    /// The buffer's running time: now minus the time since the frame was captured (from the
    /// frame's timestamp clock, else when the backend received it). Without that, frames
    /// queued while downstream blocks would be stamped late and a syncing sink would fall
    /// behind the camera.
    fn timestamp(&self, meta: &FrameMeta) -> Option<gst::ClockTime> {
        let obj = self.obj();
        let clock = obj.clock()?;
        let now = clock.time().checked_sub(obj.base_time()?)?;
        let age = meta
            .clock
            .and_then(|c| c.elapsed_since(meta.timestamp))
            .or_else(|| meta.capture_instant.map(|t| t.elapsed()))
            .unwrap_or_default();
        gst::log!(CAT, imp = self, "running time {now}, frame age {age:?}");
        Some(now.saturating_sub(gst::ClockTime::from_nseconds(age.as_nanos() as u64)))
    }

    /// Caps for a frame that does not match the negotiated caps (camera service frames).
    fn caps_for_frame(meta: &FrameMeta, negotiated: &Negotiated) -> Option<gst::Caps> {
        let res = meta.format.resolution;
        let mut s = caps::structure_for(meta.format.code)?;
        s.set("width", res.width.get() as i32);
        s.set("height", res.height.get() as i32);
        let (num, den) = negotiated.fps.unwrap_or((0, 1));
        s.set("framerate", gst::Fraction::new(num, den));
        Some(gst::Caps::from(s))
    }

    fn make_buffer(
        &self,
        state: &mut State,
        frame: FrameLease,
        dmabuf: bool,
    ) -> Result<gst::Buffer, gst::FlowError> {
        let delta = frame.meta().delta;
        let ts = self.timestamp(frame.meta());
        let kind = if dmabuf {
            MemoryKind::DmaBuf
        } else if self.settings().export_dmabuf && !state.export_failed {
            MemoryKind::PreferDmaBuf
        } else {
            MemoryKind::Wrap
        };
        let attempt = frame_buffer(frame, kind);
        let (mut buffer, backing) = match attempt {
            Ok(done) => done,
            Err(BufferError::NotExportable(why)) => {
                // Offered dma-bufs but the camera cannot export them: stop offering them and
                // renegotiate (the caller drops this frame).
                gst::info!(CAT, imp = self, "camera buffers are not dma-bufs: {why}");
                if let Some(caps) = state.caps.as_ref() {
                    let mut plain = gst::Caps::new_empty();
                    let plain_mut = plain.get_mut().expect("new caps are writable");
                    for (s, features) in caps.iter_with_features() {
                        if !features.contains(gst_allocators::CAPS_FEATURE_MEMORY_DMABUF) {
                            plain_mut
                                .append_structure_full(s.to_owned(), Some(features.to_owned()));
                        }
                    }
                    state.caps = Some(plain);
                }
                state.dmabuf_unavailable = true;
                return Err(gst::FlowError::CustomError);
            }
            Err(err) => {
                gst::warning!(CAT, imp = self, "dropping frame: {err}");
                return Err(gst::FlowError::Error);
            }
        };
        if state.frames == 0 {
            gst::info!(CAT, imp = self, "first frame as {backing:?} memory");
        }
        let duration = state
            .negotiated
            .as_ref()
            .and_then(Negotiated::frame_duration);
        let b = buffer.get_mut().expect("new buffer is writable");
        b.set_pts(ts);
        b.set_duration(duration);
        b.set_offset(state.frames);
        b.set_offset_end(state.frames + 1);
        if delta {
            b.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
        if state.frames == 0 {
            b.set_flags(gst::BufferFlags::DISCONT);
        }
        if let Backing::Fallback(why) = &backing {
            gst::info!(CAT, imp = self, "not exporting dma-bufs: {why}");
            state.export_failed = true;
        }
        state.frames += 1;
        Ok(buffer)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for StyxSrc {
    const NAME: &'static str = "GstStyxSrc";
    type Type = super::StyxSrc;
    type ParentType = gst_base::PushSrc;
}

impl ObjectImpl for StyxSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("camera")
                    .nick("Camera")
                    .blurb(
                        "Camera to open: its name, part of it, or an identity key such as \
                         /dev/video0 (first camera if unset); \"virtual\" for a test camera",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("service")
                    .nick("Camera service")
                    .blurb(
                        "Socket of a Styx camera service to get frames from instead of opening \
                         the camera in this process",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("extra-controls")
                    .nick("Extra controls")
                    .blurb(
                        "Camera controls by name, e.g. \
                         \"c,brightness=10,exposure_time_absolute=100\"",
                    )
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("priority")
                    .nick("Priority")
                    .blurb("What the planner optimises for: latency, throughput or power")
                    .default_value(Some("latency"))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("queue-depth")
                    .nick("Queue depth")
                    .blurb("Frames buffered between camera and element (0 = from priority)")
                    .maximum(8)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Fail after this many ms without a frame (0 = wait forever)")
                    .default_value(DEFAULT_TIMEOUT_MS)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("export-dmabuf")
                    .nick("Export dma-bufs")
                    .blurb(
                        "Pass camera buffers as dma-buf memory with system-memory caps too \
                         (falls back to plain memory when they cannot be exported)",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("plan")
                    .nick("Plan")
                    .blurb("What the Styx planner chose for the negotiated caps")
                    .read_only()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        match pspec.name() {
            "camera" => settings.camera = value.get().expect("type checked upstream"),
            "service" => settings.service = value.get().expect("type checked upstream"),
            "extra-controls" => {
                let controls: Option<gst::Structure> = value.get().expect("type checked upstream");
                settings.extra_controls = controls.clone();
                drop(settings);
                if let Some(controls) = controls {
                    let mut state = self.state();
                    self.apply_controls(&mut state, &controls);
                }
            }
            "priority" => {
                let priority: Option<String> = value.get().expect("type checked upstream");
                settings.priority = priority.unwrap_or_else(|| "latency".into());
            }
            "queue-depth" => settings.queue_depth = value.get().expect("type checked upstream"),
            "timeout" => settings.timeout_ms = value.get().expect("type checked upstream"),
            "export-dmabuf" => settings.export_dmabuf = value.get().expect("type checked upstream"),
            _ => unreachable!("unknown property {}", pspec.name()),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings();
        match pspec.name() {
            "camera" => settings.camera.to_value(),
            "service" => settings.service.to_value(),
            "extra-controls" => settings.extra_controls.to_value(),
            "priority" => settings.priority.to_value(),
            "queue-depth" => settings.queue_depth.to_value(),
            "timeout" => settings.timeout_ms.to_value(),
            "export-dmabuf" => settings.export_dmabuf.to_value(),
            "plan" => self.state().plan.to_value(),
            _ => unreachable!("unknown property {}", pspec.name()),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.set_live(true);
        obj.set_format(gst::Format::Time);
        obj.set_do_timestamp(false);
    }
}

impl GstObjectImpl for StyxSrc {}

impl ElementImpl for StyxSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Styx camera source",
                "Source/Video",
                "Live frames from a Styx camera (V4L2, native sensors, libcamera, camera service)",
                "Mathias Petersen",
            )
        });
        Some(&*METADATA)
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                let mut state = self.state();
                *state = State::default();
                if let Err(err) = self.open_camera(&mut state) {
                    drop(state);
                    self.post_error_message(err);
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToNull => *self.state() = State::default(),
            _ => {}
        }
        self.parent_change_state(transition)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            vec![
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps::template_caps(),
                )
                .expect("valid template"),
            ]
        });
        TEMPLATES.as_ref()
    }
}

impl BaseSrcImpl for StyxSrc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state();
        let (device, caps) = (state.device.take(), state.caps.take());
        *state = State {
            device,
            caps,
            ..State::default()
        };
        if state.device.is_none() {
            self.open_camera(&mut state)?;
        }
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state();
        Self::stop_stream(&mut state);
        let (device, caps) = (state.device.take(), state.caps.take());
        *state = State {
            device,
            caps,
            ..State::default()
        };
        Ok(())
    }

    fn caps(&self, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        let caps = self
            .state()
            .caps
            .clone()
            .unwrap_or_else(caps::template_caps);
        Some(match filter {
            Some(filter) => filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First),
            None => caps,
        })
    }

    fn fixate(&self, mut caps: gst::Caps) -> gst::Caps {
        caps.truncate();
        {
            let caps = caps.make_mut();
            if let Some(s) = caps.structure_mut(0) {
                // Ranges (camera service caps): a common size and rate.
                s.fixate_field_nearest_int("width", 1280);
                s.fixate_field_nearest_int("height", 720);
                s.fixate_field_nearest_fraction("framerate", gst::Fraction::new(30, 1));
            }
        }
        self.parent_fixate(caps)
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let negotiated = caps::negotiated(caps)
            .ok_or_else(|| gst::loggable_error!(CAT, "caps Styx cannot produce: {caps}"))?;
        let settings = self.settings();
        let mut state = self.state();
        if state.negotiated.as_ref() == Some(&negotiated) && state.stream.is_some() {
            return Ok(());
        }
        Self::stop_stream(&mut state);
        let options = StreamOptions {
            service: settings.service.as_deref(),
            camera: settings.camera.as_deref(),
            priority: settings.priority(),
            queue_depth: (settings.queue_depth > 0).then_some(settings.queue_depth as usize),
        };
        let (stream, plan) =
            Stream::start(state.device.as_ref(), &negotiated, &options).map_err(|err| {
                gst::element_imp_error!(self, gst::ResourceError::OpenRead, ("{err}"));
                gst::loggable_error!(CAT, "cannot start {caps}: {err}")
            })?;
        gst::info!(CAT, imp = self, "streaming {caps}\n{plan}");
        state.stream = Some(stream);
        state.plan = Some(plan);
        state.negotiated = Some(negotiated);
        state.last_frame = Some(Instant::now());
        if let Some(controls) = settings.extra_controls {
            self.apply_controls(&mut state, &controls);
        }
        drop(state);
        self.obj().notify("plan");
        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        if let gst::QueryViewMut::Latency(q) = query.view_mut() {
            let state = self.state();
            let Some(duration) = state
                .negotiated
                .as_ref()
                .and_then(Negotiated::frame_duration)
            else {
                return false;
            };
            let depth = u64::from(self.settings().queue_depth.max(1));
            q.set(true, duration, Some(duration * (depth + 1)));
            return true;
        }
        BaseSrcImplExt::parent_query(self, query)
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        self.flushing.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        self.flushing.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl PushSrcImpl for StyxSrc {
    fn create(
        &self,
        _buffer: Option<&mut gst::BufferRef>,
    ) -> Result<CreateSuccess, gst::FlowError> {
        let timeout = Duration::from_millis(u64::from(self.settings().timeout_ms));
        loop {
            if self.flushing.load(Ordering::SeqCst) {
                return Err(gst::FlowError::Flushing);
            }
            let mut state = self.state();
            let Some(stream) = state.stream.as_mut() else {
                return Err(gst::FlowError::NotNegotiated);
            };
            let outcome = match stream.next(POLL) {
                Ok(outcome) => outcome,
                Err(err) => {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Failed,
                        ("the camera's frames could not be prepared: {err}")
                    );
                    return Err(gst::FlowError::Error);
                }
            };
            match outcome {
                RecvOutcome::Data(frame) => {
                    state.last_frame = Some(Instant::now());
                    let Some(negotiated) = state.negotiated.clone() else {
                        return Err(gst::FlowError::NotNegotiated);
                    };
                    let res = frame.meta().format.resolution;
                    let matches = (res.width.get(), res.height.get())
                        == (negotiated.width, negotiated.height)
                        && caps::video_format(frame.meta().format.code)
                            == caps::video_format(negotiated.fourcc);
                    if !matches {
                        // A camera service delivered another size (it keeps the camera's
                        // aspect ratio and never upscales): renegotiate to what arrives.
                        let Some(new_caps) = Self::caps_for_frame(frame.meta(), &negotiated) else {
                            return Err(gst::FlowError::NotNegotiated);
                        };
                        let Some(updated) = caps::negotiated(&new_caps) else {
                            return Err(gst::FlowError::NotNegotiated);
                        };
                        gst::info!(CAT, imp = self, "frames arrive as {new_caps}");
                        state.negotiated = Some(updated);
                        drop(state);
                        if self.obj().set_caps(&new_caps).is_err() {
                            gst::element_imp_error!(
                                self,
                                gst::CoreError::Negotiation,
                                ("downstream refuses the camera's frames: {new_caps}")
                            );
                            return Err(gst::FlowError::NotNegotiated);
                        }
                        state = self.state();
                    }
                    match self.make_buffer(&mut state, frame, negotiated.dmabuf) {
                        Ok(buffer) => return Ok(CreateSuccess::NewBuffer(buffer)),
                        Err(gst::FlowError::CustomError) => {
                            drop(state);
                            self.renegotiate_without_dmabuf()?;
                        }
                        Err(err) => return Err(err),
                    }
                }
                RecvOutcome::Empty => {
                    let waited = state.last_frame.map(|t| t.elapsed()).unwrap_or_default();
                    if !timeout.is_zero() && waited >= timeout {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Read,
                            ("no frames from the camera for {} ms", waited.as_millis()),
                            ["Was the camera unplugged? Set timeout=0 to keep waiting."]
                        );
                        return Err(gst::FlowError::Error);
                    }
                }
                RecvOutcome::Closed => {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::NotFound,
                        ("the camera stopped delivering frames (unplugged or service gone)")
                    );
                    return Err(gst::FlowError::Error);
                }
            }
        }
    }
}
