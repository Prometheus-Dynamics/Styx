//! One camera's native session: the start order, the stop order, and the failure rule.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lemnos_client::{Client, RemoteCaller};
use lemnos_contract_camera_receiver::{CameraReceiver, MAX_STREAM_NAME, Packing};
use lemnos_contract_camera_sensor::{CameraSensor, Mode};
use lemnos_core::Bound;
use styx_lemnos::open_frames;
use styx_native::Camera;

use crate::backoff::{AutoOff, Policy};
use crate::clock::Clock;
use crate::frames::{Frames, Lease};
use crate::modes::{ModeError, Request, fourcc_for, select_mode};

type Sensor = Bound<dyn CameraSensor, RemoteCaller>;
type Receiver = Bound<dyn CameraReceiver, RemoteCaller>;

/// Where a session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not running, by request. [`Session::start`] brings it up.
    Stopped,
    /// Frames flow.
    Streaming,
    /// A failure; waiting out the backoff before the next attempt.
    Backoff,
    /// The policy's auto-off gave up after repeated failures. [`Session::start`] tries again.
    Off,
}

/// What a session can show about itself, read through [`Session::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// The sensor mode the session runs, while it streams.
    pub mode: Option<u16>,
    /// Successful starts.
    pub starts: u64,
    /// Failures in a row, from the last successful start.
    pub consecutive_failures: u32,
    /// Failures since the session was made.
    pub total_failures: u64,
    /// The last failure, as text.
    pub last_error: Option<String>,
    /// The wait before the next attempt, while in backoff.
    pub next_delay: Option<Duration>,
    /// When the next attempt is due, on the session clock's timeline.
    pub retry_at: Option<Duration>,
}

impl Status {
    fn new() -> Self {
        Self {
            state: State::Stopped,
            mode: None,
            starts: 0,
            consecutive_failures: 0,
            total_failures: 0,
            last_error: None,
            next_delay: None,
            retry_at: None,
        }
    }
}

/// Why a session call did not give frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Stopped by [`Session::stop`] or a [`StopHandle`].
    Stopped,
    /// The policy turned the session off after `failures` failures in a row.
    AutoOff { failures: u32, last_error: String },
    /// [`Session::next_frame`] was called while the session is not streaming.
    NotStreaming,
    /// No sensor mode fits the request. Retrying cannot fix this, so [`Session::start`] returns it
    /// at once.
    Mode(ModeError),
    /// A device refused to stop. Both were asked to stop.
    Device(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => write!(f, "stopped by request"),
            Self::AutoOff {
                failures,
                last_error,
            } => write!(
                f,
                "turned off after {failures} failures in a row: {last_error}"
            ),
            Self::NotStreaming => write!(f, "no session is streaming"),
            Self::Mode(e) => write!(f, "no usable sensor mode: {e}"),
            Self::Device(text) => write!(f, "{text}"),
        }
    }
}

impl std::error::Error for Error {}

/// Asks a session to stop from another thread.
#[derive(Debug, Clone)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    /// A backoff in progress ends at once and the session call returns [`Error::Stopped`]; no
    /// restart follows. A streaming session keeps streaming until its owner calls [`Session::stop`].
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// One attempt that did not stream: a retry, or a request no retry can fix.
enum Fault {
    Retry(String),
    Permanent(ModeError),
}

/// A lemnos call's refusal, as text for the status.
fn refused<T>(result: lemnos_core::Result<T>, call: &str) -> Result<T, Fault> {
    result.map_err(|status| Fault::Retry(format!("{call} refused: {status:?}")))
}

/// The devices of a streaming camera, and its frames.
struct Running {
    sensor: Sensor,
    receiver: Receiver,
    frames: Frames,
    mode: u16,
}

impl Running {
    /// Starts the receiver, then the sensor, then drops the mode's first frames. On a failure the
    /// caller stops both devices.
    fn start(&mut self, drop_frames: u8, frame_timeout: Duration) -> Result<(), Fault> {
        refused(self.receiver.start(), "receiver.start")?;
        refused(self.sensor.start(), "sensor.start")?;
        for dropped in 0..drop_frames {
            match self.frames.next(frame_timeout) {
                Ok(Some(lease)) => drop(lease),
                Ok(None) => {
                    return Err(Fault::Retry(format!(
                        "no frame while dropping the first {drop_frames} ({dropped} seen)"
                    )));
                }
                Err(e) => return Err(Fault::Retry(format!("frame stream: {e}"))),
            }
        }
        Ok(())
    }
}

/// Stops the sensor, then the receiver, then gives the frame stream back to Lemnos. Each is tried even
/// when an earlier one refuses. Dropping a stream without closing it would hold it until the
/// connection ends, so the next start could not open it.
fn halt(client: &Client, mut running: Running) -> Result<(), String> {
    let sensor = running
        .sensor
        .stop()
        .map_err(|status| format!("sensor.stop refused: {status:?}"));
    let receiver = running
        .receiver
        .stop()
        .map_err(|status| format!("receiver.stop refused: {status:?}"));
    let closed = client
        .close_stream(running.frames.into_stream())
        .map_err(|e| format!("closing the frame stream: {e}"));
    sensor.and(receiver).and(closed)
}

/// Stops what started, then reports the fault, with the stop's refusal if there was one.
fn unwind(client: &Client, running: Running, fault: Fault) -> Fault {
    match (halt(client, running), fault) {
        (Ok(()), fault) => fault,
        (Err(stop), Fault::Retry(text)) => Fault::Retry(format!("{text}; {stop}")),
        (Err(_), permanent) => permanent,
    }
}

/// One camera's native session.
///
/// Sessions are made over a Lemnos client and a camera from [`styx_native`]. Nothing runs until
/// [`Session::start`].
pub struct Session<'a, C: Clock> {
    client: &'a Client,
    camera: &'a Camera,
    request: Request,
    policy: Policy,
    clock: C,
    stop: Arc<AtomicBool>,
    running: Option<Running>,
    stalls: u32,
    status: Status,
}

/// A dropped session stops its devices. Its frames are released with the stream.
impl<C: Clock> Drop for Session<'_, C> {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = halt(self.client, running);
        }
    }
}

impl<C: Clock> fmt::Debug for Session<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("camera", &self.camera.name())
            .field("status", &self.status)
            .finish()
    }
}

impl<'a, C: Clock> Session<'a, C> {
    /// A session for `camera` that runs the mode `request` picks, under `policy`, timed by `clock`.
    pub fn new(
        client: &'a Client,
        camera: &'a Camera,
        request: Request,
        policy: Policy,
        clock: C,
    ) -> Self {
        Self {
            client,
            camera,
            request,
            policy,
            clock,
            stop: Arc::new(AtomicBool::new(false)),
            running: None,
            stalls: 0,
            status: Status::new(),
        }
    }

    /// What the session is doing, and the backoff if it is waiting.
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// A handle that stops this session from another thread.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle(Arc::clone(&self.stop))
    }

    /// Brings the camera up: selects the mode, configures the receiver, starts receiver then
    /// sensor, and drops the mode's first frames. A failed attempt is backed off and retried
    /// (see the module's failure rule) until it streams, the policy turns the session off, or a
    /// stop is requested.
    ///
    /// Clears a stop request and the failure count, so a stopped or turned-off session can be
    /// started again.
    pub fn start(&mut self) -> Result<(), Error> {
        self.stop.store(false, Ordering::Release);
        if self.running.is_none() {
            self.status.consecutive_failures = 0;
        }
        self.bring_up()
    }

    /// Manual off: stops the sensor, then the receiver, and ends any backoff in progress.
    pub fn stop(&mut self) -> Result<(), Error> {
        self.stop.store(true, Ordering::Release);
        self.stalls = 0;
        self.status.state = State::Stopped;
        self.status.mode = None;
        self.status.next_delay = None;
        self.status.retry_at = None;
        match self.running.take() {
            Some(running) => halt(self.client, running).map_err(Error::Device),
            None => Ok(()),
        }
    }

    /// The next frame, waiting up to `timeout`.
    ///
    /// `Ok(None)` means no frame this time (or a frame was dropped because its buffer was still
    /// held), and the caller calls again. A stream that gives no frame for `stall_waits` waits in a
    /// row, or fails, is a fault: the devices are stopped, the backoff runs, and the camera starts
    /// again inside this call. Errors are returned only for a stop, an auto-off, or no running
    /// session.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<Lease>, Error> {
        let polled = match self.running.as_mut() {
            Some(running) => running.frames.next(timeout),
            None => return Err(Error::NotStreaming),
        };
        match polled {
            Ok(Some(lease)) => {
                self.stalls = 0;
                Ok(Some(lease))
            }
            Ok(None) => {
                self.stalls += 1;
                if self.stalls < self.policy.stall_waits.max(1) {
                    return Ok(None);
                }
                let text = format!("no frame in {} waits", self.stalls);
                self.fault(text)?;
                Ok(None)
            }
            Err(styx_lemnos::Error::Held(_)) => Ok(None),
            Err(e) => {
                self.fault(format!("frame stream: {e}"))?;
                Ok(None)
            }
        }
    }

    /// Stops the devices after a fault, backs off, and starts again.
    fn fault(&mut self, text: String) -> Result<(), Error> {
        self.stalls = 0;
        let text = match self.running.take() {
            Some(running) => match halt(self.client, running) {
                Ok(()) => text,
                Err(stop) => format!("{text}; {stop}"),
            },
            None => text,
        };
        self.backoff_after(text)?;
        self.bring_up()
    }

    fn bring_up(&mut self) -> Result<(), Error> {
        if self.running.is_some() {
            return Ok(());
        }
        loop {
            if self.stop.load(Ordering::Acquire) {
                self.status.state = State::Stopped;
                return Err(Error::Stopped);
            }
            match self.attempt() {
                Ok(running) => {
                    self.status.state = State::Streaming;
                    self.status.mode = Some(running.mode);
                    self.status.starts += 1;
                    self.status.consecutive_failures = 0;
                    self.status.next_delay = None;
                    self.status.retry_at = None;
                    self.running = Some(running);
                    return Ok(());
                }
                Err(Fault::Permanent(e)) => {
                    self.status.state = State::Stopped;
                    self.status.last_error = Some(e.to_string());
                    return Err(Error::Mode(e));
                }
                Err(Fault::Retry(text)) => self.backoff_after(text)?,
            }
        }
    }

    /// Counts a failure, then either turns the session off (policy) or waits out the backoff.
    fn backoff_after(&mut self, text: String) -> Result<(), Error> {
        self.status.total_failures += 1;
        self.status.consecutive_failures += 1;
        let failures = self.status.consecutive_failures;
        self.status.last_error = Some(text.clone());
        if let AutoOff::AfterFailures(limit) = self.policy.auto_off
            && failures >= limit.get()
        {
            self.status.state = State::Off;
            self.status.next_delay = None;
            self.status.retry_at = None;
            return Err(Error::AutoOff {
                failures,
                last_error: text,
            });
        }
        let delay = self.policy.backoff.delay(failures);
        self.status.state = State::Backoff;
        self.status.next_delay = Some(delay);
        self.status.retry_at = Some(self.clock.now() + delay);
        self.clock.wait(delay, &self.stop);
        Ok(())
    }

    /// One start, in the order the Lemnos camera contracts set. Nothing is left running on a fault.
    fn attempt(&mut self) -> Result<Running, Fault> {
        let sensor_name = &self.camera.sensor().name;
        let receiver_name = &self.camera.receiver().name;
        let mut sensor: Sensor = self
            .client
            .open::<dyn CameraSensor>(sensor_name)
            .map_err(|e| Fault::Retry(format!("open {sensor_name}: {e}")))?;
        let mut receiver: Receiver = self
            .client
            .open::<dyn CameraReceiver>(receiver_name)
            .map_err(|e| Fault::Retry(format!("open {receiver_name}: {e}")))?;

        let info = refused(sensor.info(), "sensor.info")?;
        let mut modes: Vec<Mode> = Vec::with_capacity(usize::from(info.modes));
        for index in 0..info.modes {
            modes.push(refused(sensor.mode(index), "sensor.mode")?);
        }
        let index = select_mode(&modes, &self.request).map_err(Fault::Permanent)?;
        let mode = modes[usize::from(index)];
        let code = fourcc_for(&mode).map_err(Fault::Permanent)?;

        refused(sensor.select_mode(index), "sensor.select_mode")?;
        let layout = refused(
            receiver.configure(mode.link, Packing::Packed),
            "receiver.configure",
        )?;
        let mut name = [0u8; MAX_STREAM_NAME];
        let len = refused(receiver.frame_stream(&mut name), "receiver.frame_stream")?;
        let stream = name
            .get(..len)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .ok_or_else(|| Fault::Retry("receiver.frame_stream: name is not UTF-8".to_string()))?;
        let frames = open_frames(
            self.client,
            self.camera.receiver().index,
            stream,
            code,
            layout.width,
            layout.height,
        )
        .map_err(|e| Fault::Retry(format!("frame stream {stream}: {e}")))?;

        let mut running = Running {
            sensor,
            receiver,
            frames,
            mode: index,
        };
        match running.start(mode.drop_frames, self.policy.frame_timeout) {
            Ok(()) => Ok(running),
            Err(fault) => Err(unwind(self.client, running, fault)),
        }
    }
}
