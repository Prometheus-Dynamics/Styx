//! A client's camera controls: requests go over a connection of their own to the camera
//! service (opened on first use, so frames and control answers never wait for each other), with
//! the client's token so the service knows which camera and which client it is.
//!
//! [`ControlChannel`] is that connection, for frame clients ([`FrameClient`]) and control
//! clients ([`ControlClient`](super::ControlClient)) alike; `control_methods!` gives both the
//! same methods.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use styx_core::prelude::*;
use styx_graph::rt;

use super::FrameClient;
use crate::ipc::controls::{AppliedControl, ControlDescriptor, ControlEvent};
use crate::ipc::wire::{self, ControlOp, ControlReply, ControlRequest, ServerMessage};
use crate::ipc::{IpcError, socket};

/// A connection for control requests, opened on first use (and again after the service went
/// away), and where it goes.
pub(super) struct ControlChannel {
    path: PathBuf,
    timeout: Duration,
    link: Mutex<Option<ControlLink>>,
}

struct ControlLink {
    socket: OwnedFd,
    seq: u32,
}

/// Whom a control request is for: a camera (by name, else the service's first) and, for a
/// frame client, its token (the service then knows the client, and its camera).
pub(super) struct ControlTo {
    pub(super) camera: Option<String>,
    pub(super) token: Option<u64>,
}

impl ControlTo {
    pub(super) fn message(&self, seq: u32, op: ControlOp) -> ControlRequest {
        ControlRequest {
            seq,
            camera: self.camera.clone(),
            token: self.token,
            op,
        }
    }
}

/// What the focus lens follows ([`FrameClient::set_af_mode`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfMode {
    /// The lens stays where [`FrameClient::set_lens_position`] puts it.
    Manual = 0,
    /// A scan per [`FrameClient::trigger_af`].
    Auto = 1,
    /// Focus continuously.
    Continuous = 2,
}

fn timed_out() -> IpcError {
    IpcError::Io(std::io::ErrorKind::TimedOut.into())
}

fn reset() -> IpcError {
    IpcError::Io(std::io::ErrorKind::ConnectionReset.into())
}

type Answer = (ControlReply, Option<OwnedFd>);

/// A kept connection the service closed (it went away, or restarted): sending fails with a
/// broken pipe, receiving with a reset. Tried again on a fresh connection.
fn stale(err: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionReset, NotConnected};
    matches!(err.kind(), BrokenPipe | ConnectionReset | NotConnected)
}

impl ControlChannel {
    pub(super) fn new(path: &Path, timeout: Duration) -> Self {
        Self {
            path: path.to_path_buf(),
            timeout,
            link: Mutex::new(None),
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Send `op` and wait for its answer (and the memfd of a list), up to the timeout. Fails at
    /// once when the service is not there.
    pub(super) fn request(&self, to: &ControlTo, op: ControlOp) -> Result<Answer, IpcError> {
        let mut guard = self.link.lock();
        // Once more on a fresh connection if the service went away (and came back) meanwhile.
        for attempt in 0..2 {
            let deadline = Instant::now() + self.timeout;
            if guard.is_none() {
                *guard = Some(ControlLink {
                    socket: socket::connect_until(&self.path, deadline)?,
                    seq: 0,
                });
            }
            let link = guard.as_mut().expect("connected above");
            link.seq = link.seq.wrapping_add(1);
            let message = to.message(link.seq, op.clone());
            match exchange(&link.socket, &message, deadline) {
                Err(IpcError::Io(err)) if attempt == 0 && stale(&err) => {
                    *guard = None;
                }
                Err(err) => {
                    *guard = None;
                    return Err(err);
                }
                Ok(answer) => return Ok(answer),
            }
        }
        Err(reset())
    }

    /// [`ControlChannel::request`] on any executor: connecting and the answer are awaited
    /// through styx-graph's reactor, never blocking a thread. Requests made at the same time
    /// each take a connection of their own.
    pub(super) async fn request_async(
        &self,
        to: &ControlTo,
        op: ControlOp,
    ) -> Result<Answer, IpcError> {
        for attempt in 0..2 {
            let taken = self.link.lock().take();
            let fresh = taken.is_none();
            let mut link = match taken {
                Some(link) => link,
                None => ControlLink {
                    socket: rt::timeout(self.timeout, connect_async(&self.path))
                        .await
                        .map_err(|_| timed_out())??,
                    seq: 0,
                },
            };
            link.seq = link.seq.wrapping_add(1);
            let message = to.message(link.seq, op.clone());
            match rt::timeout(self.timeout, exchange_async(&link.socket, &message)).await {
                Err(_) => return Err(timed_out()),
                Ok(Err(IpcError::Io(err))) if attempt == 0 && !fresh && stale(&err) => {}
                Ok(Err(err)) => return Err(err),
                Ok(Ok(answer)) => {
                    let mut slot = self.link.lock();
                    if slot.is_none() {
                        *slot = Some(link);
                    }
                    return Ok(answer);
                }
            }
        }
        Err(reset())
    }

    /// Subscribe to control changes on a connection of its own, waiting up to the timeout.
    pub(super) fn subscribe(&self, to: &ControlTo) -> Result<ControlEvents, IpcError> {
        let deadline = Instant::now() + self.timeout;
        let socket = socket::connect_until(&self.path, deadline)?;
        match exchange(&socket, &to.message(1, ControlOp::Subscribe), deadline)?.0 {
            ControlReply::Subscribed => Ok(ControlEvents { socket }),
            ControlReply::Refused(refusal) => Err(IpcError::ControlRefused(refusal)),
            _ => Err(IpcError::Malformed("expected a subscription")),
        }
    }
}

/// The answer to a set.
pub(super) fn applied(answer: Answer) -> Result<AppliedControl, IpcError> {
    match answer.0 {
        ControlReply::Applied(applied) => Ok(applied),
        ControlReply::Refused(refusal) => Err(IpcError::ControlRefused(refusal)),
        _ => Err(IpcError::Malformed("expected an applied control")),
    }
}

/// The answer to a get.
pub(super) fn value(answer: Answer) -> Result<ControlValue, IpcError> {
    match answer.0 {
        ControlReply::Value(_, value) => Ok(value),
        ControlReply::Refused(refusal) => Err(IpcError::ControlRefused(refusal)),
        _ => Err(IpcError::Malformed("expected a control value")),
    }
}

/// The answer to a list: the list, read from its memfd.
pub(super) fn list(answer: Answer) -> Result<Vec<ControlDescriptor>, IpcError> {
    match answer {
        (ControlReply::List(_, len), Some(fd)) => {
            if len > wire::MAX_CONTROL_LIST {
                return Err(IpcError::Malformed("control list too long"));
            }
            let mut body = vec![0u8; len];
            std::fs::File::from(fd).read_exact_at(&mut body, 0)?;
            wire::decode_control_list(&body)
        }
        (ControlReply::Refused(refusal), _) => Err(IpcError::ControlRefused(refusal)),
        _ => Err(IpcError::Malformed(
            "expected a control list with its memfd",
        )),
    }
}

/// The control methods of a client with a `ControlChannel` (`self.control`) and a
/// `fn control_to(&self) -> Result<ControlTo, IpcError>`: the same for frame and control
/// clients.
macro_rules! control_methods {
    () => {
        /// Change a camera control on the shared camera: a backend's control id (see
        /// [`Self::controls`]) or a [`StandardControl`](crate::ipc::StandardControl) (in its
        /// units, whatever the backend). The answer says what is in effect now (clamped to
        /// the control's range when the value was outside it), `deferred` while the camera is
        /// stopped (applied when it starts), and, on frame-exact backends, the first frame
        /// using it; every client that subscribed is told. Refused
        /// ([`IpcError::ControlRefused`]) when the camera has no such control, it is read
        /// only, the value is unusable, or the service's
        /// [`ControlPolicy`](crate::ipc::ControlPolicy) does not let this client change it.
        /// Waits for the service's answer (up to the client's timeout; at once an error when
        /// the service is not there); [`Self::set_control_async`] awaits it instead.
        pub fn set_control(
            &self,
            control: impl Into<crate::ipc::ControlTarget>,
            value: ControlValue,
        ) -> Result<crate::ipc::AppliedControl, IpcError> {
            let op = crate::ipc::wire::ControlOp::Set(control.into(), value);
            crate::ipc::client::control::applied(self.control.request(&self.control_to()?, op)?)
        }

        /// [`Self::set_control`] on any executor (through styx-graph's reactor): never
        /// blocks a thread.
        pub async fn set_control_async(
            &self,
            control: impl Into<crate::ipc::ControlTarget>,
            value: ControlValue,
        ) -> Result<crate::ipc::AppliedControl, IpcError> {
            let op = crate::ipc::wire::ControlOp::Set(control.into(), value);
            let to = self.control_to()?;
            crate::ipc::client::control::applied(self.control.request_async(&to, op).await?)
        }

        /// A camera control's value now (a [`StandardControl`](crate::ipc::StandardControl)
        /// in its units).
        pub fn get_control(
            &self,
            control: impl Into<crate::ipc::ControlTarget>,
        ) -> Result<ControlValue, IpcError> {
            let op = crate::ipc::wire::ControlOp::Get(control.into());
            crate::ipc::client::control::value(self.control.request(&self.control_to()?, op)?)
        }

        /// [`Self::get_control`] on any executor.
        pub async fn get_control_async(
            &self,
            control: impl Into<crate::ipc::ControlTarget>,
        ) -> Result<ControlValue, IpcError> {
            let op = crate::ipc::wire::ControlOp::Get(control.into());
            let to = self.control_to()?;
            crate::ipc::client::control::value(self.control.request_async(&to, op).await?)
        }

        /// The camera's controls: id, name, type, range, default, menu, the value now, the
        /// standard control each answers, and whether this client may change it (for a
        /// settings screen). A frame rate the service sets by restarting the capture is
        /// listed as [`SERVICE_FRAME_RATE`](crate::ipc::SERVICE_FRAME_RATE).
        pub fn controls(&self) -> Result<Vec<crate::ipc::ControlDescriptor>, IpcError> {
            let op = crate::ipc::wire::ControlOp::List;
            crate::ipc::client::control::list(self.control.request(&self.control_to()?, op)?)
        }

        /// [`Self::controls`] on any executor.
        pub async fn controls_async(&self) -> Result<Vec<crate::ipc::ControlDescriptor>, IpcError> {
            let op = crate::ipc::wire::ControlOp::List;
            let to = self.control_to()?;
            crate::ipc::client::control::list(self.control.request_async(&to, op).await?)
        }

        /// Exposure time in microseconds (turn automatic exposure off for it to hold).
        pub fn set_exposure_us(&self, us: u32) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(
                crate::ipc::StandardControl::ExposureUs,
                ControlValue::Uint(us),
            )
        }

        /// Total gain as a ratio (1.0: none).
        pub fn set_gain(&self, gain: f32) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(crate::ipc::StandardControl::Gain, ControlValue::Float(gain))
        }

        /// Automatic exposure on or off.
        pub fn set_ae(&self, on: bool) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(
                crate::ipc::StandardControl::AeEnable,
                ControlValue::Bool(on),
            )
        }

        /// Exposure compensation in stops.
        pub fn set_ev(&self, stops: f32) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(
                crate::ipc::StandardControl::ExposureValue,
                ControlValue::Float(stops),
            )
        }

        /// Frame rate: through the camera's control where it has one, else by restarting the
        /// capture for every client at the rate (if the service's policy allows).
        pub fn set_fps(&self, fps: f32) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(
                crate::ipc::StandardControl::FrameRate,
                ControlValue::Float(fps),
            )
        }

        /// Automatic white balance on or off.
        pub fn set_awb(&self, on: bool) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(
                crate::ipc::StandardControl::AwbEnable,
                ControlValue::Bool(on),
            )
        }

        /// White balance colour temperature in kelvin (used while AWB is off).
        pub fn set_colour_temperature(
            &self,
            kelvin: u32,
        ) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(
                crate::ipc::StandardControl::ColourTemperature,
                ControlValue::Uint(kelvin),
            )
        }

        /// Manual red and blue gains, relative to green (used while AWB is off).
        pub fn set_colour_gains(
            &self,
            red: f32,
            blue: f32,
        ) -> Result<(crate::ipc::AppliedControl, crate::ipc::AppliedControl), IpcError> {
            Ok((
                self.set_control(
                    crate::ipc::StandardControl::RedGain,
                    ControlValue::Float(red),
                )?,
                self.set_control(
                    crate::ipc::StandardControl::BlueGain,
                    ControlValue::Float(blue),
                )?,
            ))
        }

        /// What drives the focus lens.
        pub fn set_af_mode(
            &self,
            mode: crate::ipc::AfMode,
        ) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(
                crate::ipc::StandardControl::AfMode,
                ControlValue::Int(mode as i32),
            )
        }

        /// Start an autofocus scan ([`AfMode::Auto`](crate::ipc::AfMode::Auto)).
        pub fn trigger_af(&self) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(crate::ipc::StandardControl::AfTrigger, ControlValue::Int(0))
        }

        /// Cancel an autofocus scan.
        pub fn cancel_af(&self) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(crate::ipc::StandardControl::AfTrigger, ControlValue::Int(1))
        }

        /// Lens position in dioptres (0: infinity), in
        /// [`AfMode::Manual`](crate::ipc::AfMode::Manual).
        pub fn set_lens_position(
            &self,
            dioptres: f32,
        ) -> Result<crate::ipc::AppliedControl, IpcError> {
            self.set_control(
                crate::ipc::StandardControl::LensPosition,
                ControlValue::Float(dioptres),
            )
        }
    };
}
pub(super) use control_methods;

impl FrameClient {
    /// Whom this client's control requests are for: its camera, and its token once connected.
    fn control_to(&self) -> Result<ControlTo, IpcError> {
        let Some(request) = &self.request else {
            return Err(IpcError::Rejected(
                "controls are served by a camera service, not a frame server".into(),
            ));
        };
        Ok(ControlTo {
            camera: request.lock().camera.clone(),
            token: self.link.lock().token.map(|t| t.token),
        })
    }

    control_methods!();

    /// Follow control changes on this client's camera (by any client, this one too): a
    /// connection of its own, readable ([`AsFd`]) when an event is waiting.
    pub fn control_events(&self) -> Result<ControlEvents, IpcError> {
        self.control.subscribe(&self.control_to()?)
    }
}

/// Connect without blocking a thread (a full listener backlog is retried).
async fn connect_async(path: &Path) -> Result<OwnedFd, IpcError> {
    let connecting = socket::Connecting::new(path)?;
    while !connecting.attempt()? {
        rt::sleep(Duration::from_millis(2)).await;
    }
    Ok(connecting.into_socket())
}

/// [`exchange`] awaiting the answer (the caller bounds the time).
async fn exchange_async(socket: &OwnedFd, request: &ControlRequest) -> Result<Answer, IpcError> {
    if !socket::send(socket, &wire::encode_control(request), &[])? {
        return Err(timed_out());
    }
    loop {
        match socket::recv(socket, Duration::ZERO)? {
            socket::Received::Message(bytes, fds) => {
                if let Some(answer) = answered(request.seq, &bytes, fds)? {
                    return Ok(answer);
                }
            }
            socket::Received::Nothing => {
                rt::readable(socket.as_fd()).await?;
            }
            socket::Received::Closed => return Err(reset()),
        }
    }
}

/// The answer to request `seq` in a message, if it is that; events and older answers are
/// skipped.
fn answered(seq: u32, bytes: &[u8], mut fds: Vec<OwnedFd>) -> Result<Option<Answer>, IpcError> {
    match wire::decode_server(bytes)? {
        ServerMessage::ControlReply(s, reply) if s == seq => {
            Ok(Some((reply, (!fds.is_empty()).then(|| fds.swap_remove(0)))))
        }
        ServerMessage::Reject(reason) => Err(IpcError::Rejected(reason)),
        _ => Ok(None),
    }
}

/// Send `request` and wait until `deadline` for its answer (events in between are skipped).
fn exchange(
    socket: &OwnedFd,
    request: &ControlRequest,
    deadline: Instant,
) -> Result<Answer, IpcError> {
    if !socket::send(socket, &wire::encode_control(request), &[])? {
        return Err(timed_out());
    }
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err(timed_out());
        }
        match socket::recv(socket, wait)? {
            socket::Received::Message(bytes, fds) => {
                if let Some(answer) = answered(request.seq, &bytes, fds)? {
                    return Ok(answer);
                }
            }
            socket::Received::Nothing => {}
            socket::Received::Closed => {
                return Err(IpcError::Io(std::io::ErrorKind::ConnectionReset.into()));
            }
        }
    }
}

/// Control changes on a camera ([`FrameClient::control_events`]). Readable ([`AsFd`]) when an
/// event is waiting.
pub struct ControlEvents {
    socket: OwnedFd,
}

impl ControlEvents {
    pub(super) fn into_socket(self) -> OwnedFd {
        self.socket
    }

    /// The next change, waiting up to `wait`; `Closed` once the service is gone.
    pub fn recv(&self, wait: Duration) -> RecvOutcome<ControlEvent> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match socket::recv(&self.socket, left) {
                Ok(socket::Received::Message(bytes, _)) => {
                    if let Ok(ServerMessage::ControlEvent(event)) = wire::decode_server(&bytes) {
                        return RecvOutcome::Data(event);
                    }
                }
                Ok(socket::Received::Nothing) => return RecvOutcome::Empty,
                Ok(socket::Received::Closed) | Err(_) => return RecvOutcome::Closed,
            }
            if left.is_zero() {
                return RecvOutcome::Empty;
            }
        }
    }

    /// The next change if one is waiting.
    pub fn try_recv(&self) -> RecvOutcome<ControlEvent> {
        self.recv(Duration::ZERO)
    }
}

impl AsFd for ControlEvents {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl AsRawFd for ControlEvents {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}
