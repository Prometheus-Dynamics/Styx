//! An open camera, through either source.
//!
//! In process, the camera's capture lives on its own worker thread, which owns the Lemnos client
//! and the session the capture borrows. The C caller's calls become commands to that thread: a
//! frame request (the worker waits for the next frame, up to the timeout) and a control update.
//! Over the socket, the camera is a control connection to the service and a `styx-socket` reader
//! of the camera's frame socket; both are owned here and need no thread.
//!
//! Frame rate is the achieved rate since this camera was opened: the frame ids and timestamps of
//! the first frame delivered against the newest, in milli-frames per second. It is 0 until the
//! second frame.

use std::num::NonZeroU32;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use styx_controls::{Control, ControlValue, DeviceKind, FrameMetadata, Update, ValueError};
use styx_frame::FrameLease;
use styx_lemnos::Client;
use styx_native::discover;
use styx_native_run::{AutoOff, Initial, Policy, Request, Session, SystemClock};
use styx_select::adapters::NativeCapture;
use styx_select::{Backend, Capture, CaptureError, ControlsInfo, DeviceInfo, Refusal, Selected};
use styx_socket::{Reader, SharedLayout, SocketError};

use crate::frame::Frame;
use crate::source::{self, CLIENT_NAME, Kind, Source};
use crate::status::{
    E_BUSY, E_IO, E_NOT_FOUND, E_PROTOCOL, E_RANGE, E_STOPPED, E_UNSUPPORTED, Fail, LAYOUT_MARKS,
    LAYOUT_RING, Res,
};
use crate::wire::{self, ControlLink, IO_WAIT};

/// How long opening a camera may take before it fails.
const OPEN_WAIT: Duration = Duration::from_secs(10);
/// How long one frame wait on the worker lasts at most inside the capture.
const FRAME_WAIT: Duration = Duration::from_millis(100);
/// The exposure and gain the in-process path starts with when the caller gives none (the
/// service's defaults).
const DEFAULT_EXPOSURE_US: u32 = 10_000;
const DEFAULT_GAIN_MILLI: u32 = 1_000;

/// What the C caller asks of an open camera. Zero means the default. The in-process path needs
/// `fps_milli`; the service's config decides everything, so the service path ignores these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenOptions {
    pub width: u32,
    pub height: u32,
    pub fps_milli: u32,
    pub exposure_us: u32,
    pub gain_milli: u32,
}

/// A camera's controls as one answer: what `styx_camera_controls` hands out. `seq` is the camera's
/// controls count when it was made (the service's `controls_seq`; in process, the capture's
/// controls generation): a caller asks again when it moves.
#[derive(Debug, Clone)]
pub struct Controls {
    pub seq: u64,
    pub info: Arc<ControlsInfo>,
}

/// One open camera.
#[derive(Debug)]
pub struct Camera {
    name: String,
    link: Link,
    /// The first frame's id and timestamp, for the achieved rate.
    first: Option<(u64, u64)>,
    /// The newest frame's metadata: the controls in force on it.
    last: Option<FrameMetadata>,
}

#[derive(Debug)]
enum Link {
    Local(LocalLink),
    Remote(Box<RemoteLink>),
}

impl Camera {
    /// Opens the camera named `name` from `source`.
    pub(crate) fn open(source: &Source, name: &str, options: OpenOptions) -> Res<Self> {
        if name.is_empty() {
            return Err(Fail::invalid("an empty camera name"));
        }
        let options = OpenOptions {
            exposure_us: if options.exposure_us == 0 {
                DEFAULT_EXPOSURE_US
            } else {
                options.exposure_us
            },
            gain_milli: if options.gain_milli == 0 {
                DEFAULT_GAIN_MILLI
            } else {
                options.gain_milli
            },
            ..options
        };
        let link = match &source.kind {
            Kind::Local { lemnos } => Link::Local(LocalLink::start(lemnos, name, options)?),
            Kind::Service { control } => {
                if name.split_ascii_whitespace().count() != 1 {
                    return Err(Fail::invalid(
                        "a camera name over the service is one word (the service's list has one per line)",
                    ));
                }
                Link::Remote(Box::new(RemoteLink::open(control, name)?))
            }
        };
        Ok(Self {
            name: name.to_string(),
            link,
            first: None,
            last: None,
        })
    }

    /// Sets a control, applied from frame `frame` on.
    pub(crate) fn set_control(
        &mut self,
        control: Control,
        value: ControlValue,
        frame: u64,
    ) -> Res<()> {
        control.validate(value).map_err(|e| match e {
            ValueError::OutOfRange { min, max, value } => Fail::new(
                E_RANGE,
                format!("{value} is outside {min}..{max} for {}", control.name()),
            ),
            other => Fail::invalid(format!(
                "{} cannot take that value: {other:?}",
                control.name()
            )),
        })?;
        match &mut self.link {
            Link::Local(link) => {
                // The service checks against the camera's own ranges; in process, the same check.
                let controls = link.controls()?;
                controls
                    .info
                    .standard(control)
                    .ok_or_else(|| no_control(control))?
                    .check(value)
                    .map_err(wire::refusal)?;
                link.set(Update {
                    control,
                    value,
                    frame,
                })
            }
            Link::Remote(link) => link.set(&self.name, control, value, frame),
        }
    }

    /// The value in force on the newest frame.
    pub(crate) fn get_control(&mut self, control: Control) -> Res<ControlValue> {
        match &mut self.link {
            Link::Local(link) => {
                if link.controls()?.info.standard(control).is_none() {
                    return Err(no_control(control));
                }
                self.last
                    .as_ref()
                    .map(|meta| meta.control(control))
                    .ok_or_else(|| Fail::new(E_NOT_FOUND, "no frame yet, so no value"))
            }
            Link::Remote(link) => {
                let lines = link.control.ask(&format!("controls {}", self.name))?;
                wire::value_of(&lines, control)
            }
        }
    }

    /// The camera's controls now: the standard ones with this camera's ranges and every device
    /// control.
    pub(crate) fn controls(&mut self) -> Res<Controls> {
        match &mut self.link {
            Link::Local(link) => link.controls(),
            Link::Remote(link) => {
                let lines = link.control.ask(&format!("controls {}", self.name))?;
                let (seq, info) = wire::controls_of(&lines);
                Ok(Controls {
                    seq,
                    info: Arc::new(info),
                })
            }
        }
    }

    /// Device control `id`'s value, in the device's units. `E_NOT_FOUND` for an id the camera does
    /// not have, or a value the backend cannot say.
    pub(crate) fn get_device(&mut self, id: u32) -> Res<i64> {
        let device = match &mut self.link {
            Link::Local(link) => link.controls()?.info.device(id).cloned(),
            Link::Remote(link) => {
                let lines = link.control.ask(&format!("getd {} {id:#x}", self.name))?;
                wire::controls_of(&lines)
                    .1
                    .device
                    .into_iter()
                    .find(|d| d.id == id)
            }
        };
        let device = device.ok_or_else(|| no_device(id))?;
        device.value.ok_or_else(|| {
            Fail::new(
                E_NOT_FOUND,
                format!("{}: the backend does not say its value", device.name),
            )
        })
    }

    /// Sets device control `id` to `value`, in the device's units, checked against what the camera
    /// says of the control.
    pub(crate) fn set_device(&mut self, id: u32, value: i64) -> Res<()> {
        match &mut self.link {
            Link::Local(link) => {
                let controls = link.controls()?;
                let device: &DeviceInfo = controls.info.device(id).ok_or_else(|| no_device(id))?;
                device.check(value).map_err(wire::refusal)?;
                link.set_device(id, value)
            }
            Link::Remote(link) => link
                .control
                .ask(&format!("setd {} {id:#x} {value}", self.name))
                .map(|_| ()),
        }
    }

    /// Sets float device control `id` to `value`, checked against what the camera says of it: a
    /// control that is not a float, or a value that is not finite, is the wrong kind.
    pub(crate) fn set_device_real(&mut self, id: u32, value: f32) -> Res<()> {
        if !value.is_finite() {
            return Err(wire::refusal(Refusal::Kind));
        }
        match &mut self.link {
            Link::Local(link) => {
                let controls = link.controls()?;
                let device: &DeviceInfo = controls.info.device(id).ok_or_else(|| no_device(id))?;
                if device.kind != DeviceKind::Float {
                    return Err(wire::refusal(Refusal::Kind));
                }
                if device.access.refuses_set() {
                    return Err(wire::refusal(Refusal::Access(device.access)));
                }
                device
                    .real
                    .unwrap_or_default()
                    .check(value)
                    .map_err(wire::refusal)?;
                link.set_device_real(id, value)
            }
            Link::Remote(link) => link
                .control
                .ask(&format!(
                    "setd {} {id:#x} {}",
                    self.name,
                    styx_select::real_text(value)
                ))
                .map(|_| ()),
        }
    }

    /// The next frame, or `None` when none came within `timeout`.
    pub(crate) fn next_frame(&mut self, timeout: Duration) -> Res<Option<Frame>> {
        let frame = match &mut self.link {
            Link::Local(link) => link.next(timeout)?.map(Frame::local),
            Link::Remote(link) => link.next(timeout)?.map(Frame::remote).transpose()?,
        };
        let Some(mut frame) = frame else {
            return Ok(None);
        };
        let fps = self.track(frame.id(), frame.timestamp_ns());
        frame.set_fps(fps);
        self.last = Some(*frame.meta());
        Ok(Some(frame))
    }

    /// The achieved rate since the first frame, in milli-frames per second.
    fn track(&mut self, id: u64, timestamp_ns: u64) -> u32 {
        let (first_id, first_ts) = *self.first.get_or_insert((id, timestamp_ns));
        let (Some(frames), Some(ns)) =
            (id.checked_sub(first_id), timestamp_ns.checked_sub(first_ts))
        else {
            return 0;
        };
        if frames == 0 || ns == 0 {
            return 0;
        }
        u32::try_from(u128::from(frames) * 1_000_000_000_000 / u128::from(ns)).unwrap_or(u32::MAX)
    }
}

fn no_control(control: Control) -> Fail {
    Fail::new(
        E_NOT_FOUND,
        format!("the camera has no control {}", control.name()),
    )
}

fn no_device(id: u32) -> Fail {
    Fail::new(
        E_NOT_FOUND,
        format!("the camera has no device control {id:#x}"),
    )
}

// In-process path -----------------------------------------------------------------------------

enum Command {
    Next {
        timeout: Duration,
        reply: Sender<Result<Option<FrameLease<FrameMetadata>>, CaptureError>>,
    },
    Set {
        update: Update,
        reply: Sender<Result<(), CaptureError>>,
    },
    SetDevice {
        id: u32,
        value: i64,
        reply: Sender<Result<(), CaptureError>>,
    },
    SetDeviceReal {
        id: u32,
        value: f32,
        reply: Sender<Result<(), CaptureError>>,
    },
    Controls {
        reply: Sender<Controls>,
    },
}

/// A camera's worker thread and the channel to it.
#[derive(Debug)]
struct LocalLink {
    commands: Option<Sender<Command>>,
    worker: Option<JoinHandle<()>>,
}

impl LocalLink {
    fn start(lemnos: &Path, name: &str, options: OpenOptions) -> Res<Self> {
        let selected = source::plan(lemnos)?
            .cameras
            .into_iter()
            .find(|s| s.id == name)
            .ok_or_else(|| Fail::new(E_NOT_FOUND, format!("no camera named {name}")))?;
        if selected.owner != Backend::Native {
            return Err(Fail::new(
                E_UNSUPPORTED,
                format!(
                    "camera {name} is {}: the in-process path opens native cameras (the service serves the others)",
                    selected.owner
                ),
            ));
        }
        if options.fps_milli == 0 {
            return Err(Fail::invalid(
                "fps_milli is required for an in-process camera",
            ));
        }
        let (commands, receiver) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let lemnos = lemnos.to_path_buf();
        let worker = std::thread::Builder::new()
            .name(format!("styx-capi:{name}"))
            .spawn(move || work(&selected, &lemnos, options, &receiver, &ready))
            .map_err(Fail::io)?;
        match started.recv_timeout(OPEN_WAIT) {
            Ok(Ok(())) => Ok(Self {
                commands: Some(commands),
                worker: Some(worker),
            }),
            Ok(Err(fail)) => {
                let _ = worker.join();
                Err(fail)
            }
            Err(_) => Err(Fail::new(E_IO, "the camera did not start in time")),
        }
    }

    fn send(&self, command: Command) -> Res<()> {
        self.commands
            .as_ref()
            .ok_or_else(|| Fail::new(E_STOPPED, "the camera is closed"))?
            .send(command)
            .map_err(|_| Fail::new(E_STOPPED, "the camera's worker has ended"))
    }

    fn next(&mut self, timeout: Duration) -> Res<Option<FrameLease<FrameMetadata>>> {
        let (reply, answer) = mpsc::channel();
        self.send(Command::Next { timeout, reply })?;
        answer
            .recv_timeout(timeout + IO_WAIT)
            .map_err(|_| Fail::new(E_IO, "the camera did not answer"))?
            .map_err(next_error)
    }

    fn set(&mut self, update: Update) -> Res<()> {
        let (reply, answer) = mpsc::channel();
        self.send(Command::Set { update, reply })?;
        answer
            .recv_timeout(IO_WAIT)
            .map_err(|_| Fail::new(E_IO, "the camera did not answer"))?
            .map_err(set_error)
    }

    fn set_device(&mut self, id: u32, value: i64) -> Res<()> {
        let (reply, answer) = mpsc::channel();
        self.send(Command::SetDevice { id, value, reply })?;
        answer
            .recv_timeout(IO_WAIT)
            .map_err(|_| Fail::new(E_IO, "the camera did not answer"))?
            .map_err(set_error)
    }

    fn set_device_real(&mut self, id: u32, value: f32) -> Res<()> {
        let (reply, answer) = mpsc::channel();
        self.send(Command::SetDeviceReal { id, value, reply })?;
        answer
            .recv_timeout(IO_WAIT)
            .map_err(|_| Fail::new(E_IO, "the camera did not answer"))?
            .map_err(set_error)
    }

    /// The camera's controls, as the worker last built them (again only when they moved).
    fn controls(&mut self) -> Res<Controls> {
        let (reply, answer) = mpsc::channel();
        self.send(Command::Controls { reply })?;
        answer
            .recv_timeout(IO_WAIT)
            .map_err(|_| Fail::new(E_IO, "the camera did not answer"))
    }
}

impl Drop for LocalLink {
    fn drop(&mut self) {
        // The worker ends when its commands do; then its capture stops and the daemon releases it.
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn next_error(error: CaptureError) -> Fail {
    match error {
        CaptureError::Stopped => Fail::new(E_STOPPED, "the camera stopped"),
        CaptureError::Failed(text) | CaptureError::Contract { text, .. } => {
            Fail::new(E_STOPPED, format!("the camera failed: {text}"))
        }
        CaptureError::Unsupported(what) => Fail::new(E_UNSUPPORTED, what),
    }
}

fn set_error(error: CaptureError) -> Fail {
    match error {
        CaptureError::Stopped => Fail::new(E_STOPPED, "the camera stopped"),
        CaptureError::Failed(text) | CaptureError::Contract { text, .. } => Fail::new(E_IO, text),
        CaptureError::Unsupported(what) => Fail::new(E_UNSUPPORTED, what),
    }
}

/// The worker: discovers the camera, starts its capture, and answers commands until the camera
/// is closed. The session's policy turns a fault into a stopped capture; the caller opens again.
fn work(
    selected: &Selected,
    lemnos: &Path,
    options: OpenOptions,
    commands: &Receiver<Command>,
    ready: &Sender<Result<(), Fail>>,
) {
    let client = match Client::connect(lemnos, CLIENT_NAME) {
        Ok(client) => client,
        Err(e) => {
            let _ = ready.send(Err(Fail::io(format!("lemnos {}: {e}", lemnos.display()))));
            return;
        }
    };
    let found = match discover(&client) {
        Ok(found) => found,
        Err(e) => {
            let _ = ready.send(Err(Fail::io(format!("lemnos discovery: {e}"))));
            return;
        }
    };
    let Some(camera) = found.cameras.iter().find(|c| c.name() == selected.key) else {
        let _ = ready.send(Err(Fail::new(
            E_NOT_FOUND,
            format!("native camera {} is not on the daemon", selected.key),
        )));
        return;
    };
    let request = Request {
        width: options.width,
        height: options.height,
        fps_milli: options.fps_milli,
        format: None,
        bit_depth: None,
        bayer: None,
    };
    let initial = Initial {
        exposure_us: options.exposure_us,
        gain_milli: options.gain_milli,
        frame_rate_milli: options.fps_milli,
    };
    let policy = Policy {
        auto_off: AutoOff::AfterFailures(NonZeroU32::MIN),
        frame_timeout: FRAME_WAIT,
        ..Policy::default()
    };
    let session = Session::new(&client, camera, request, policy, SystemClock::default())
        .with_controls(initial);
    let mut capture = match NativeCapture::start(session) {
        Ok(capture) => capture,
        Err(e) => {
            let _ = ready.send(Err(Fail::io(e)));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    // The controls answer, built again only when the capture's generation moves: a caller that
    // asks per frame shares one.
    let mut controls: Option<Controls> = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Next { timeout, reply } => {
                let _ = reply.send(capture.next_frame(timeout));
            }
            Command::Set { update, reply } => {
                let _ = reply.send(capture.set_controls(&[update]));
            }
            Command::SetDevice { id, value, reply } => {
                let _ = reply.send(capture.set_device(id, value));
            }
            Command::SetDeviceReal { id, value, reply } => {
                let _ = reply.send(capture.set_device_real(id, value));
            }
            Command::Controls { reply } => {
                let seq = capture.controls_generation();
                let current = match controls.take() {
                    Some(held) if held.seq == seq => held,
                    _ => Controls {
                        seq,
                        info: Arc::new(capture.controls_info()),
                    },
                };
                let _ = reply.send(current.clone());
                controls = Some(current);
            }
        }
    }
}

// Socket path ---------------------------------------------------------------------------------

/// A control connection to the service and a reader of the camera's frame socket.
#[derive(Debug)]
struct RemoteLink {
    control: ControlLink,
    reader: Reader,
}

impl RemoteLink {
    /// Connects to the camera's frame socket once the service publishes it. A camera that is not
    /// publishing yet (or whose reader the service closes while it starts) is tried again until
    /// the open wait is over.
    fn open(control_path: &Path, name: &str) -> Res<Self> {
        let mut control = ControlLink::connect(control_path)?;
        let deadline = Instant::now() + OPEN_WAIT;
        loop {
            let entries = wire::entries(&control.ask("list")?);
            let entry = entries
                .into_iter()
                .find(|e| e.name == name)
                .ok_or_else(|| {
                    Fail::new(E_NOT_FOUND, format!("the service has no camera {name}"))
                })?;
            let mut last = String::from("not publishing");
            if let Some(socket) = entry.socket {
                match connect_reader(&socket) {
                    Ok(reader) => return Ok(Self { control, reader }),
                    Err(fail) => last = fail.message,
                }
            }
            if Instant::now() >= deadline {
                return Err(Fail::new(
                    E_BUSY,
                    format!("camera {name} is not publishing frames: {last}"),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn set(&mut self, name: &str, control: Control, value: ControlValue, frame: u64) -> Res<()> {
        let text = wire::value_text(value)?;
        self.control
            .ask(&format!("set {name} {} {text} {frame}", control.name()))
            .map(|_| ())
    }

    /// The next frame the reader holds, waiting up to `timeout` for one.
    fn next(&mut self, timeout: Duration) -> Res<Option<styx_socket::Frame>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.reader.next_frame() {
                Ok(Some(frame)) => return Ok(Some(frame)),
                Ok(None) => {}
                Err(e @ styx_socket::ReadError::Ended(styx_socket::StreamEnd::ConsumerFault)) => {
                    // A cut-off reader: its frames are no longer valid to read; release them first.
                    return Err(Fail::new(
                        E_STOPPED,
                        format!("{e}; release every frame you hold, then open the camera again"),
                    ));
                }
                Err(e) => {
                    return Err(Fail::new(E_STOPPED, format!("{e}; open the camera again")));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            self.reader.wait(Some(deadline - now)).map_err(Fail::io)?;
        }
    }
}

fn connect_reader(path: &Path) -> Res<Reader> {
    let stream: UnixStream = UnixStream::connect(path)
        .map_err(|e| Fail::io(format!("frame socket {}: {e}", path.display())))?;
    stream.set_read_timeout(Some(IO_WAIT)).map_err(Fail::io)?;
    let reader = Reader::connect(stream).map_err(|e| socket_fail(e, path))?;
    Ok(reader)
}

/// A handshake's refusal as the call's status. A layout refusal keeps its structure and both
/// values (`STYX_E_LAYOUT`, read with `styx_layout_refusal`); a version refusal is a protocol
/// mismatch whose message names both versions; anything else is an i/o failure.
fn socket_fail(error: SocketError, path: &Path) -> Fail {
    let text = format!("frame socket {}: {error}", path.display());
    match error {
        SocketError::Layout {
            which,
            found,
            expected,
        } => Fail::layout(
            match which {
                SharedLayout::Ring => LAYOUT_RING,
                SharedLayout::Marks => LAYOUT_MARKS,
            },
            text,
            found,
            expected,
        ),
        SocketError::Version { .. } => Fail::new(E_PROTOCOL, text),
        _ => Fail::io(text),
    }
}

#[cfg(test)]
mod socket_error_tests {
    use super::*;
    use crate::status::{E_LAYOUT, E_PROTOCOL, Layout};

    #[test]
    fn a_frame_socket_layout_refusal_is_the_layout_status_with_its_values() {
        let fail = socket_fail(
            SocketError::Layout {
                which: SharedLayout::Marks,
                found: 1,
                expected: 2,
            },
            Path::new("/run/styx/frames.sock"),
        );
        assert_eq!(fail.status, E_LAYOUT);
        assert_eq!(
            fail.layout,
            Some(Layout {
                which: LAYOUT_MARKS,
                found: 1,
                expected: 2,
            })
        );
        assert!(fail.message.contains("layout 1") && fail.message.contains("layout 2"));
    }

    #[test]
    fn a_version_refusal_is_a_protocol_mismatch_naming_both_versions() {
        let fail = socket_fail(SocketError::Version { ours: 5, theirs: 4 }, Path::new("/s"));
        assert_eq!(fail.status, E_PROTOCOL);
        assert_eq!(fail.layout, None);
        assert!(fail.message.contains("version 4") && fail.message.contains("version 5"));
    }
}
