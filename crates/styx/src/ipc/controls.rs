//! Camera controls over a camera service's socket: what a client asks for, what the service
//! answers, who may change what ([`ControlPolicy`]), and the change events every subscribed
//! client gets.

use std::sync::Arc;

use styx_core::prelude::*;

use super::PeerCredentials;

/// A camera control every backend names differently, in one set of units. The service finds
/// the backend's control: by its snake-case name (native cameras and virtual ones, e.g.
/// `exposure_time_us`), its libcamera name (`ExposureTime`) or its V4L2/UVC id (converted:
/// V4L2 exposure counts 100 µs; gains and lens positions are in the device's units there).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StandardControl {
    /// Exposure time in microseconds (`Uint`).
    ExposureUs = 1,
    /// Total gain as a ratio (`Float`).
    Gain = 2,
    /// Automatic exposure on or off (`Bool`).
    AeEnable = 3,
    /// Exposure compensation in stops (`Float`).
    ExposureValue = 4,
    /// Frame rate in frames per second (`Float`). Where the camera cannot change it while
    /// streaming, the service restarts the capture for every client at the new rate (if its
    /// [`ControlPolicy`] allows) or refuses.
    FrameRate = 5,
    /// Automatic white balance on or off (`Bool`).
    AwbEnable = 6,
    /// Colour temperature in kelvin (`Uint`), used while AWB is off.
    ColourTemperature = 7,
    /// Red gain relative to green (`Float`), used while AWB is off.
    RedGain = 8,
    /// Blue gain relative to green (`Float`), used while AWB is off.
    BlueGain = 9,
    /// What drives the focus lens (`Int`): 0 manual, 1 auto (a scan per trigger), 2 continuous.
    AfMode = 10,
    /// Auto mode: 0 starts a scan, 1 cancels it (`Int`).
    AfTrigger = 11,
    /// Lens position in dioptres (`Float`; 0 is infinity), set in manual mode.
    LensPosition = 12,
}

impl StandardControl {
    pub const ALL: [StandardControl; 12] = [
        Self::ExposureUs,
        Self::Gain,
        Self::AeEnable,
        Self::ExposureValue,
        Self::FrameRate,
        Self::AwbEnable,
        Self::ColourTemperature,
        Self::RedGain,
        Self::BlueGain,
        Self::AfMode,
        Self::AfTrigger,
        Self::LensPosition,
    ];

    pub(super) fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|c| *c as u8 == code)
    }

    /// The snake-case name native and virtual cameras give it.
    pub fn name(self) -> &'static str {
        match self {
            Self::ExposureUs => "exposure_time_us",
            Self::Gain => "gain",
            Self::AeEnable => "ae_enable",
            Self::ExposureValue => "exposure_value",
            Self::FrameRate => "frame_rate",
            Self::AwbEnable => "awb_enable",
            Self::ColourTemperature => "colour_temperature",
            Self::RedGain => "red_gain",
            Self::BlueGain => "blue_gain",
            Self::AfMode => "af_mode",
            Self::AfTrigger => "af_trigger",
            Self::LensPosition => "lens_position",
        }
    }

    /// Its libcamera name, where libcamera has it as one scalar control.
    pub(crate) fn libcamera_name(self) -> Option<&'static str> {
        Some(match self {
            Self::ExposureUs => "ExposureTime",
            Self::Gain => "AnalogueGain",
            Self::AeEnable => "AeEnable",
            Self::ExposureValue => "ExposureValue",
            Self::AwbEnable => "AwbEnable",
            Self::ColourTemperature => "ColourTemperature",
            Self::AfMode => "AfMode",
            Self::AfTrigger => "AfTrigger",
            Self::LensPosition => "LensPosition",
            Self::FrameRate | Self::RedGain | Self::BlueGain => return None,
        })
    }
}

/// Which control a request is about: a backend's own id, or a [`StandardControl`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ControlTarget {
    Id(ControlId),
    Standard(StandardControl),
}

impl From<ControlId> for ControlTarget {
    fn from(id: ControlId) -> Self {
        Self::Id(id)
    }
}

impl From<StandardControl> for ControlTarget {
    fn from(control: StandardControl) -> Self {
        Self::Standard(control)
    }
}

/// A control change the service accepted, and what is in effect now.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedControl {
    /// The backend's control (for a frame rate the service applied by restarting the capture
    /// with no backend control for it: [`SERVICE_FRAME_RATE`]).
    pub id: ControlId,
    /// The value asked for.
    pub requested: ControlValue,
    /// The value in effect: read back from the camera, or the value applied when it cannot be
    /// read. In the standard control's units for a [`StandardControl`] request.
    pub value: ControlValue,
    /// The value asked for was outside the control's range (or between its steps) and was
    /// clamped to `value`.
    pub clamped: bool,
    /// Frame-exact backends (a native camera's raw modes, and a processed mode's fixed exposure
    /// or gain): the first frame using the value, as its sensor sequence
    /// (`NativeFrameMeta::sequence`); frames carry the exposure and gain they used in their
    /// metadata. A prediction on every sensor (see `docs/native-stack/pipeline.md`, controls):
    /// the sensor's delay from the frame in progress, not a read-back. `None` where no frame is
    /// fixed (AE, EV, AWB, and 0 handing a control back to AE).
    pub frame: Option<u64>,
    /// The camera is not streaming: the value is applied when it starts.
    pub deferred: bool,
    /// The capture was restarted for every client to apply it (a frame rate the camera cannot
    /// change while streaming).
    pub restarted: bool,
}

/// Why the service refused a control request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlRefusal {
    /// The camera has no such control (or none the standard control maps to).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The control can be read, not set.
    #[error("read only: {0}")]
    ReadOnly(String),
    /// The value cannot be used (wrong type, not a menu entry, not clampable).
    #[error("invalid value: {0}")]
    Invalid(String),
    /// The service's [`ControlPolicy`] does not let this client change it.
    #[error("not permitted: {0}")]
    NotPermitted(String),
    /// The camera refused it, or applying it failed.
    #[error("failed: {0}")]
    Failed(String),
}

/// A camera control as the service lists it: the backend's description, its value now, and
/// the standard control it answers, if any.
#[derive(Clone, Debug)]
pub struct ControlDescriptor {
    pub meta: ControlMeta,
    /// Its value now (`None`: it cannot be read).
    pub current: Option<ControlValue>,
    pub standard: Option<StandardControl>,
    /// The service's policy lets this client change it.
    pub writable: bool,
}

/// A control a client of the camera changed, sent to every client that subscribed
/// ([`FrameClient::control_events`](super::FrameClient::control_events)), the one that changed
/// it too.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlEvent {
    pub id: ControlId,
    pub standard: Option<StandardControl>,
    /// The value in effect, in the standard control's units when `standard` is set.
    pub value: ControlValue,
    /// Frame-exact backends: the first frame using it.
    pub frame: Option<u64>,
    /// The client that changed it ([`FrameClient::client_id`](super::FrameClient::client_id));
    /// `None` for a client without frames.
    pub by: Option<u64>,
}

/// The id an [`AppliedControl`] carries for a frame rate the service set by restarting the
/// capture (the camera has no control for it).
pub const SERVICE_FRAME_RATE: ControlId = ControlId(0xF5F0_0001);

/// Who may change a camera's controls when several clients share it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ControlWriters {
    /// Any client (the default). Conflicting writes: the last one wins, and every subscribed
    /// client is told.
    #[default]
    Anyone,
    /// Only the camera's owner: the client connected to it longest (the next one when it
    /// leaves). Clients without frames cannot change controls.
    Owner,
    /// Nobody: controls can be read and listed only.
    Nobody,
}

type Allow = Arc<dyn Fn(&ControlCaller, ControlId) -> bool + Send + Sync>;

/// Who is asking to change a control, for [`ControlPolicy::allow`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlCaller {
    /// The process, as the kernel reports it.
    pub peer: Option<PeerCredentials>,
    /// Its frame client on this camera ([`FrameClient::client_id`](super::FrameClient::client_id)).
    pub client: Option<u64>,
    /// It is the camera's owner (connected to it longest).
    pub owner: bool,
}

/// The rules a [`CameraService`](super::CameraService) applies to control changes
/// ([`CameraService::control_policy`](super::CameraService::control_policy)). By default any
/// client may change any writable control, the last write wins, every subscribed client is
/// told, and a frame rate the camera cannot change while streaming restarts the capture.
#[derive(Clone, Default)]
pub struct ControlPolicy {
    writers: ControlWriters,
    read_only: Vec<ControlTarget>,
    allow: Option<Allow>,
    no_restart: bool,
}

impl std::fmt::Debug for ControlPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPolicy")
            .field("writers", &self.writers)
            .field("read_only", &self.read_only)
            .field("allow", &self.allow.is_some())
            .field("no_restart", &self.no_restart)
            .finish()
    }
}

impl ControlPolicy {
    /// Any client may change controls (the default).
    pub fn anyone() -> Self {
        Self::default()
    }

    /// Only the camera's owner (the client connected to it longest) may change controls.
    pub fn owner_only() -> Self {
        Self {
            writers: ControlWriters::Owner,
            ..Self::default()
        }
    }

    /// No client may change controls; they can be read and listed.
    pub fn read_only() -> Self {
        Self {
            writers: ControlWriters::Nobody,
            ..Self::default()
        }
    }

    /// This control may not be changed by clients (a backend id or a standard control).
    pub fn read_only_control(mut self, control: impl Into<ControlTarget>) -> Self {
        self.read_only.push(control.into());
        self
    }

    /// Changes also need `allow` to accept them, given the caller and the backend's control id
    /// (an allow-list, e.g. `|caller, _| caller.peer.is_some_and(|p| p.uid == 0)`).
    pub fn allow(
        mut self,
        allow: impl Fn(&ControlCaller, ControlId) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.allow = Some(Arc::new(allow));
        self
    }

    /// Refuse frame rates the camera cannot change while streaming instead of restarting the
    /// capture for every client.
    pub fn no_restart(mut self) -> Self {
        self.no_restart = true;
        self
    }

    pub fn writers(&self) -> ControlWriters {
        self.writers
    }

    pub(super) fn restarts(&self) -> bool {
        !self.no_restart
    }

    /// Whether `caller` may change `id` (reached through `standard`, if so); why not.
    pub(super) fn check(
        &self,
        caller: &ControlCaller,
        id: ControlId,
        standard: Option<StandardControl>,
    ) -> Result<(), ControlRefusal> {
        let refuse = |why: &str| Err(ControlRefusal::NotPermitted(why.into()));
        match self.writers {
            ControlWriters::Anyone => {}
            ControlWriters::Owner if caller.owner => {}
            ControlWriters::Owner => {
                return refuse("only the camera's owner (its longest-connected client) may");
            }
            ControlWriters::Nobody => return refuse("the service's controls are read only"),
        }
        let listed = |t: &ControlTarget| match t {
            ControlTarget::Id(i) => *i == id,
            ControlTarget::Standard(s) => Some(*s) == standard,
        };
        if self.read_only.iter().any(listed) {
            return refuse("the service keeps this control read only");
        }
        if let Some(allow) = &self.allow
            && !allow(caller, id)
        {
            return refuse("the service's allow-list does not accept this client");
        }
        Ok(())
    }
}
