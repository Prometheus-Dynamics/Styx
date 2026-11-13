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
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use styx_controls::{Control, ControlValue, FrameMetadata, Update, ValueError};
use styx_frame::FrameLease;
use styx_lemnos::Client;
use styx_native::discover;
use styx_native_run::{AutoOff, Initial, Policy, Request, Session, SystemClock};
use styx_select::adapters::NativeCapture;
use styx_select::{Backend, Capture, CaptureError, Selected};
use styx_socket::Reader;

use crate::frame::Frame;
use crate::source::{self, CLIENT_NAME, Kind, Source};
use crate::status::{E_BUSY, E_IO, E_NOT_FOUND, E_RANGE, E_STOPPED, E_UNSUPPORTED, Fail, Res};
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
            Link::Local(link) => link.set(Update {
                control,
                value,
                frame,
            }),
            Link::Remote(link) => link.set(&self.name, control, value, frame),
        }
    }

    /// The value in force on the newest frame.
    pub(crate) fn get_control(&mut self, control: Control) -> Res<ControlValue> {
        match &mut self.link {
            Link::Local(_) => self
                .last
                .as_ref()
                .map(|meta| meta.control(control))
                .ok_or_else(|| Fail::new(E_NOT_FOUND, "no frame yet, so no value")),
            Link::Remote(link) => {
                let lines = link.control.ask(&format!("controls {}", self.name))?;
                wire::value_of(&lines, control)
            }
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
        CaptureError::Failed(text) => Fail::new(E_STOPPED, format!("the camera failed: {text}")),
        CaptureError::Unsupported(what) => Fail::new(E_UNSUPPORTED, what),
    }
}

fn set_error(error: CaptureError) -> Fail {
    match error {
        CaptureError::Stopped => Fail::new(E_STOPPED, "the camera stopped"),
        CaptureError::Failed(text) => Fail::new(E_IO, text),
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
    while let Ok(command) = commands.recv() {
        match command {
            Command::Next { timeout, reply } => {
                let _ = reply.send(capture.next_frame(timeout));
            }
            Command::Set { update, reply } => {
                let _ = reply.send(capture.set_controls(&[update]));
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
    let reader = Reader::connect(stream)
        .map_err(|e| Fail::io(format!("frame socket {}: {e}", path.display())))?;
    Ok(reader)
}
