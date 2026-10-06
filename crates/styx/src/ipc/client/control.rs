//! A client's camera controls: requests go over a connection of their own to the camera
//! service (opened on first use, so frames and control answers never wait for each other), with
//! the client's token so the service knows which camera and which client it is.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use styx_core::prelude::*;

use super::FrameClient;
use crate::ipc::controls::{
    AppliedControl, ControlDescriptor, ControlEvent, ControlTarget, StandardControl,
};
use crate::ipc::wire::{self, ControlOp, ControlReply, ControlRequest, ServerMessage};
use crate::ipc::{IpcError, socket};

/// The control connection of a [`FrameClient`].
pub(super) struct ControlLink {
    socket: OwnedFd,
    seq: u32,
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

impl FrameClient {
    /// The request for `op`, for this client's camera.
    fn control_message(&self, seq: u32, op: ControlOp) -> Result<ControlRequest, IpcError> {
        let Some(request) = &self.request else {
            return Err(IpcError::Rejected(
                "controls are served by a camera service, not a frame server".into(),
            ));
        };
        Ok(ControlRequest {
            seq,
            camera: request.lock().camera.clone(),
            token: self.link.lock().token.map(|t| t.token),
            op,
        })
    }

    /// Send `op` on the control connection and wait for its answer (and the memfd of a list).
    fn control(&self, op: ControlOp) -> Result<(ControlReply, Option<OwnedFd>), IpcError> {
        let mut guard = self.control.lock();
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
            let message = self.control_message(link.seq, op.clone())?;
            match exchange(&link.socket, &message, deadline) {
                Err(IpcError::Io(err))
                    if attempt == 0 && err.kind() == std::io::ErrorKind::ConnectionReset =>
                {
                    *guard = None;
                }
                Err(err) => {
                    *guard = None;
                    return Err(err);
                }
                Ok(answer) => return Ok(answer),
            }
        }
        Err(IpcError::Io(std::io::ErrorKind::ConnectionReset.into()))
    }

    /// Change a camera control on the shared camera: a backend's control id (see
    /// [`FrameClient::controls`]) or a [`StandardControl`] (in its units, whatever the
    /// backend). The answer says what is in effect now (clamped to the control's range when
    /// the value was outside it) and, on frame-exact backends, the first frame using it; every
    /// client that subscribed ([`FrameClient::control_events`]) is told. Refused
    /// ([`IpcError::ControlRefused`]) when the camera has no such control, it is read only,
    /// the value is unusable, or the service's [`ControlPolicy`](crate::ipc::ControlPolicy)
    /// does not let this client change it.
    pub fn set_control(
        &self,
        control: impl Into<ControlTarget>,
        value: ControlValue,
    ) -> Result<AppliedControl, IpcError> {
        match self.control(ControlOp::Set(control.into(), value))?.0 {
            ControlReply::Applied(applied) => Ok(applied),
            ControlReply::Refused(refusal) => Err(IpcError::ControlRefused(refusal)),
            _ => Err(IpcError::Malformed("expected an applied control")),
        }
    }

    /// A camera control's value now (a [`StandardControl`] in its units).
    pub fn get_control(&self, control: impl Into<ControlTarget>) -> Result<ControlValue, IpcError> {
        match self.control(ControlOp::Get(control.into()))?.0 {
            ControlReply::Value(_, value) => Ok(value),
            ControlReply::Refused(refusal) => Err(IpcError::ControlRefused(refusal)),
            _ => Err(IpcError::Malformed("expected a control value")),
        }
    }

    /// The camera's controls: id, name, type, range, default, menu, the value now, the
    /// standard control each answers, and whether this client may change it (for a settings
    /// screen). A frame rate the service sets by restarting the capture is listed as
    /// [`SERVICE_FRAME_RATE`](crate::ipc::SERVICE_FRAME_RATE).
    pub fn controls(&self) -> Result<Vec<ControlDescriptor>, IpcError> {
        match self.control(ControlOp::List)? {
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

    /// Follow control changes on this client's camera (by any client, this one too): a
    /// connection of its own, readable ([`AsFd`]) when an event is waiting.
    pub fn control_events(&self) -> Result<ControlEvents, IpcError> {
        let deadline = Instant::now() + self.timeout;
        let socket = socket::connect_until(&self.path, deadline)?;
        let message = self.control_message(1, ControlOp::Subscribe)?;
        match exchange(&socket, &message, deadline)?.0 {
            ControlReply::Subscribed => Ok(ControlEvents { socket }),
            ControlReply::Refused(refusal) => Err(IpcError::ControlRefused(refusal)),
            _ => Err(IpcError::Malformed("expected a subscription")),
        }
    }

    /// Exposure time in microseconds (turn automatic exposure off for it to hold).
    pub fn set_exposure_us(&self, us: u32) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::ExposureUs, ControlValue::Uint(us))
    }

    /// Total gain as a ratio (1.0: none).
    pub fn set_gain(&self, gain: f32) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::Gain, ControlValue::Float(gain))
    }

    /// Automatic exposure on or off.
    pub fn set_ae(&self, on: bool) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::AeEnable, ControlValue::Bool(on))
    }

    /// Exposure compensation in stops.
    pub fn set_ev(&self, stops: f32) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::ExposureValue, ControlValue::Float(stops))
    }

    /// Frame rate: through the camera's control where it has one, else by restarting the
    /// capture for every client at the rate (if the service's policy allows).
    pub fn set_fps(&self, fps: f32) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::FrameRate, ControlValue::Float(fps))
    }

    /// Automatic white balance on or off.
    pub fn set_awb(&self, on: bool) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::AwbEnable, ControlValue::Bool(on))
    }

    /// White balance colour temperature in kelvin (used while AWB is off).
    pub fn set_colour_temperature(&self, kelvin: u32) -> Result<AppliedControl, IpcError> {
        self.set_control(
            StandardControl::ColourTemperature,
            ControlValue::Uint(kelvin),
        )
    }

    /// Manual red and blue gains, relative to green (used while AWB is off).
    pub fn set_colour_gains(
        &self,
        red: f32,
        blue: f32,
    ) -> Result<(AppliedControl, AppliedControl), IpcError> {
        Ok((
            self.set_control(StandardControl::RedGain, ControlValue::Float(red))?,
            self.set_control(StandardControl::BlueGain, ControlValue::Float(blue))?,
        ))
    }

    /// What drives the focus lens.
    pub fn set_af_mode(&self, mode: AfMode) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::AfMode, ControlValue::Int(mode as i32))
    }

    /// Start an autofocus scan ([`AfMode::Auto`]).
    pub fn trigger_af(&self) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::AfTrigger, ControlValue::Int(0))
    }

    /// Cancel an autofocus scan.
    pub fn cancel_af(&self) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::AfTrigger, ControlValue::Int(1))
    }

    /// Lens position in dioptres (0: infinity), in [`AfMode::Manual`].
    pub fn set_lens_position(&self, dioptres: f32) -> Result<AppliedControl, IpcError> {
        self.set_control(StandardControl::LensPosition, ControlValue::Float(dioptres))
    }
}

/// Send `request` and wait until `deadline` for its answer (events in between are skipped).
fn exchange(
    socket: &OwnedFd,
    request: &ControlRequest,
    deadline: Instant,
) -> Result<(ControlReply, Option<OwnedFd>), IpcError> {
    if !socket::send(socket, &wire::encode_control(request), &[])? {
        return Err(timed_out());
    }
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err(timed_out());
        }
        match socket::recv(socket, wait)? {
            socket::Received::Message(bytes, mut fds) => match wire::decode_server(&bytes)? {
                ServerMessage::ControlReply(seq, reply) if seq == request.seq => {
                    return Ok((reply, (!fds.is_empty()).then(|| fds.swap_remove(0))));
                }
                ServerMessage::Reject(reason) => return Err(IpcError::Rejected(reason)),
                _ => {}
            },
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
