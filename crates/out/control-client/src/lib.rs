//! A client of the camera service's control socket.
//!
//! The protocol is `styx-service`'s (its `control` module documents every request and answer);
//! this crate speaks it and reads the answers with the service's own parsers, so a field the
//! service adds later is skipped here rather than breaking a client.
//!
//! - **One connection per request.** The service serves one connection at a time and hangs up on a
//!   connection idle for 2 s, so a client that kept one open would hold every other client off and
//!   then find it closed. Each call connects, asks, reads the answer and closes.
//! - **A timeout per request** ([`Options::timeout`], default 5 s): connecting, writing and reading
//!   the whole answer together. A request that runs out is [`ErrorKind::Timeout`].
//! - **A retry backoff while the service is away.** A failed connect is
//!   [`ErrorKind::Unreachable`], and calls before the next try is due fail at once with the same
//!   kind and the wait left, without touching the socket. The wait starts at
//!   [`Options::retry_first`] (250 ms), doubles, and is capped at [`Options::retry_cap`] (5 s). A
//!   connect that succeeds resets it. Nothing gives up: the next call after the wait tries again.
//! - **Waiting for a change of controls:** [`ControlClient::wait_controls_change`] polls the
//!   camera's `controls_seq` every [`Options::poll`] (default 100 ms, never under [`MIN_POLL`],
//!   20 ms) until it moves or the wait is over. The service has no push; this is the one poll
//!   every consumer would otherwise write.
//! - **Requests are checked before they are sent:** a camera id or control name with white space or
//!   a control character, or longer than the service takes, is [`ErrorKind::Invalid`], so no value
//!   can add a request of its own.
//!
//! Refusals come back typed: [`ErrorKind::Range`] for a value outside the camera's range or steps,
//! [`ErrorKind::Access`] for a read-only, grabbed or disabled control, [`ErrorKind::NotFound`] for
//! a camera or control the service does not have, with the service's text (and the camera's
//! numbers) in [`Error::message`].

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use styx_controls::{Control, ControlValue};
use styx_select::{ControlsInfo, DeviceInfo, Refusal, StandardInfo};
use styx_service::control::{
    CameraLine, ControlLine, ControlsAnswer, DeviceLine, Listing, MAX_ID, MAX_LINE, Shown,
    parse_controls_answer, parse_list,
};

/// The most answer lines one request may have. A `controls` answer has one line per control and per
/// menu entry: a webcam's is some 60 lines, a libcamera camera's a few hundred.
pub const MAX_ANSWER: usize = 4096;

/// The longest answer line read. A `list` camera line is a few kilobytes.
pub const MAX_ANSWER_LINE: usize = 64 * 1024;

/// The shortest poll period [`ControlClient::wait_controls_change`] uses, whatever the options say.
pub const MIN_POLL: Duration = Duration::from_millis(20);

/// How a client talks to the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The longest one request may take: connect, write and the whole answer.
    pub timeout: Duration,
    /// The first wait after a failed connect before the socket is tried again.
    pub retry_first: Duration,
    /// The longest wait between tries; the wait doubles up to it.
    pub retry_cap: Duration,
    /// How often [`ControlClient::wait_controls_change`] asks; raised to [`MIN_POLL`].
    pub poll: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            retry_first: Duration::from_millis(250),
            retry_cap: Duration::from_secs(5),
            poll: Duration::from_millis(100),
        }
    }
}

/// What went wrong, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The service has no such camera or control, or no frame yet.
    NotFound,
    /// The camera refused the value: outside its range, between its steps, or no such menu entry.
    Range,
    /// The control cannot be set now (read-only, grabbed or disabled).
    Access,
    /// The request is malformed: a value of the wrong kind, an id or name that cannot be sent.
    Invalid,
    /// The camera is not streaming, or the service closed the connection mid-answer.
    Stopped,
    /// The backend does not support the request.
    Unsupported,
    /// The control socket could not be reached; the message says when it is tried again.
    Unreachable,
    /// The request took longer than [`Options::timeout`].
    Timeout,
    /// Reading or writing the socket failed.
    Io,
    /// The answer broke the protocol (too many lines, a line too long, not UTF-8).
    Protocol,
    /// Any other refusal by the service, in its own words.
    Service,
}

/// A failed request: its kind and the reason in words (the service's own, when it refused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The service's `err <why>`, by kind.
    pub fn from_service(why: &str) -> Self {
        let kind = if why.starts_with("no camera")
            || why.starts_with("the camera has no control")
            || why.starts_with("the camera has no device control")
            || why.starts_with("no frame yet")
        {
            ErrorKind::NotFound
        } else if why.starts_with("OutOfRange")
            || why.starts_with("NotStep")
            || why.starts_with("NoEntry")
        {
            ErrorKind::Range
        } else if why.starts_with("the control is ") {
            ErrorKind::Access
        } else if why.starts_with("the value is the wrong kind") || why.starts_with("no control") {
            ErrorKind::Invalid
        } else if why.contains("not streaming") || why.contains("absent") || why.contains("ended") {
            ErrorKind::Stopped
        } else if why.contains("unsupported") {
            ErrorKind::Unsupported
        } else {
            ErrorKind::Service
        };
        Self::new(kind, format!("control socket: {why}"))
    }

    /// A camera's refusal of a value (the in-process check), by the same kinds as the service's.
    pub fn refusal(refusal: Refusal) -> Self {
        let kind = match refusal {
            Refusal::Access(_) => ErrorKind::Access,
            Refusal::OutOfRange { .. }
            | Refusal::NotStep { .. }
            | Refusal::NoEntry { .. }
            | Refusal::RealOutOfRange { .. } => ErrorKind::Range,
            Refusal::Kind => ErrorKind::Invalid,
        };
        Self::new(kind, refusal.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The answer to `frame <id>`: the last frame the camera made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastFrame {
    pub id: u64,
    /// `CLOCK_MONOTONIC` nanoseconds.
    pub timestamp_ns: u64,
    /// The exposure the frame was made with, in microseconds (`None` when the line has none).
    pub exposure_us: Option<u64>,
    /// The analogue gain, in thousandths.
    pub gain_milli: Option<u64>,
}

/// A client of one service's control socket. It holds no connection between calls.
#[derive(Debug, Clone)]
pub struct ControlClient {
    path: PathBuf,
    options: Options,
    wait: Duration,
    not_before: Option<Instant>,
}

impl ControlClient {
    /// A client of the control socket at `path`, with the default options. Nothing is connected
    /// until the first request.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_options(path, Options::default())
    }

    /// A client with `options`: the poll period is raised to [`MIN_POLL`], and the retry cap to the
    /// first wait.
    pub fn with_options(path: impl Into<PathBuf>, mut options: Options) -> Self {
        options.poll = options.poll.max(MIN_POLL);
        options.retry_cap = options.retry_cap.max(options.retry_first);
        Self {
            path: path.into(),
            options,
            wait: options.retry_first,
            not_before: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn options(&self) -> Options {
        self.options
    }

    /// The time left before a connect is tried again, `None` when the next call tries at once.
    pub fn retry_in(&self) -> Option<Duration> {
        self.not_before
            .and_then(|at| at.checked_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
    }

    /// Sends one request line and returns its answer lines, without the closing `ok`. An `err`
    /// answer is the [`Error`] of its text. The request must be one line the service takes.
    pub fn ask(&mut self, request: &str) -> Result<Vec<String>, Error> {
        if request.len() > MAX_LINE || request.contains(['\n', '\r']) {
            return Err(Error::new(
                ErrorKind::Invalid,
                format!("a request is one line of at most {MAX_LINE} bytes"),
            ));
        }
        if let Some(left) = self.retry_in() {
            return Err(Error::new(
                ErrorKind::Unreachable,
                format!(
                    "control socket {}: unreachable; tried again in {} ms",
                    self.path.display(),
                    left.as_millis()
                ),
            ));
        }
        let deadline = Instant::now() + self.options.timeout;
        let stream = match UnixStream::connect(&self.path) {
            Ok(stream) => {
                self.wait = self.options.retry_first;
                self.not_before = None;
                stream
            }
            Err(e) => {
                let wait = self.wait;
                self.not_before = Some(Instant::now() + wait);
                self.wait = (wait * 2).min(self.options.retry_cap);
                return Err(Error::new(
                    ErrorKind::Unreachable,
                    format!(
                        "control socket {}: {e}; tried again in {} ms",
                        self.path.display(),
                        wait.as_millis()
                    ),
                ));
            }
        };
        exchange(stream, request, deadline)
    }

    /// Every camera the service has (`list`).
    pub fn list(&mut self) -> Result<Listing, Error> {
        Ok(parse_list(&self.ask("list")?))
    }

    /// One camera's `list` line.
    pub fn camera(&mut self, id: &str) -> Result<CameraLine, Error> {
        word(id, "camera id")?;
        self.list()?
            .cameras
            .into_iter()
            .find(|camera| camera.id == id)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("the service has no camera {id}"),
                )
            })
    }

    /// A camera's controls (`controls <id>`) as the service's reader gives them.
    pub fn controls(&mut self, id: &str) -> Result<ControlsAnswer, Error> {
        word(id, "camera id")?;
        Ok(parse_controls_answer(&self.ask(&format!("controls {id}"))?))
    }

    /// A camera's controls read back into the in-process types, with the answer's `controls_seq`
    /// (0 when the service gives none). A control name this build does not know is skipped.
    pub fn controls_info(&mut self, id: &str) -> Result<(u64, ControlsInfo), Error> {
        Ok(info_of(&self.controls(id)?))
    }

    /// The value of `control` on the camera's last frame.
    pub fn get(&mut self, id: &str, control: Control) -> Result<ControlValue, Error> {
        let answer = self.controls(id)?;
        value_of(&answer, control)
    }

    /// Device control `device` (`getd`), in the device's units.
    pub fn device(&mut self, id: &str, device: u32) -> Result<DeviceInfo, Error> {
        word(id, "camera id")?;
        let lines = self.ask(&format!("getd {id} {device:#x}"))?;
        info_of(&parse_controls_answer(&lines))
            .1
            .device
            .into_iter()
            .find(|d| d.id == device)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("the camera has no device control {device:#x}"),
                )
            })
    }

    /// Sets a standard control from frame `frame` on.
    pub fn set(
        &mut self,
        id: &str,
        control: Control,
        value: ControlValue,
        frame: u64,
    ) -> Result<(), Error> {
        let text = value_text(value)?;
        self.set_named(id, control.name(), &text, frame)
    }

    /// Sets a control by its name on the socket, for the names that are not standard controls
    /// (the 3A loop's: `ae`, `ev`, `awb`, `colour_temperature`, `flicker`, ...). `value` is a
    /// number, `auto`, `on` or `off`.
    pub fn set_named(
        &mut self,
        id: &str,
        name: &str,
        value: &str,
        frame: u64,
    ) -> Result<(), Error> {
        word(id, "camera id")?;
        word(name, "control name")?;
        word(value, "value")?;
        self.ask(&format!("set {id} {name} {value} {frame}"))
            .map(|_| ())
    }

    /// Sets device control `device` to `value`, in the device's units.
    pub fn set_device(&mut self, id: &str, device: u32, value: i64) -> Result<(), Error> {
        word(id, "camera id")?;
        self.ask(&format!("setd {id} {device:#x} {value}"))
            .map(|_| ())
    }

    /// Sets a float device control. A value that is not finite is the wrong kind.
    pub fn set_device_real(&mut self, id: &str, device: u32, value: f32) -> Result<(), Error> {
        if !value.is_finite() {
            return Err(Error::refusal(Refusal::Kind));
        }
        word(id, "camera id")?;
        self.ask(&format!(
            "setd {id} {device:#x} {}",
            styx_select::real_text(value)
        ))
        .map(|_| ())
    }

    /// The camera's last frame (`frame <id>`). [`ErrorKind::NotFound`] before its first frame.
    pub fn frame(&mut self, id: &str) -> Result<LastFrame, Error> {
        word(id, "camera id")?;
        let lines = self.ask(&format!("frame {id}"))?;
        lines
            .iter()
            .find_map(|line| parse_frame(line))
            .ok_or_else(|| Error::new(ErrorKind::Protocol, "the answer has no frame line"))
    }

    /// Starts the camera by hand.
    pub fn start(&mut self, id: &str) -> Result<(), Error> {
        word(id, "camera id")?;
        self.ask(&format!("start {id}")).map(|_| ())
    }

    /// Stops the camera by hand (the manual off).
    pub fn stop(&mut self, id: &str) -> Result<(), Error> {
        word(id, "camera id")?;
        self.ask(&format!("stop {id}")).map(|_| ())
    }

    /// Waits until the camera's `controls_seq` is other than `seen` (the `seq` of the last
    /// [`controls`](Self::controls) answer read), asking every [`Options::poll`]. Returns the new
    /// count, or `None` when `timeout` passes with no change.
    ///
    /// A service that is away, restarting or slow is asked again at the next poll, within the
    /// wait: such an error is returned only when no poll succeeded at all. A camera the service
    /// does not have, and a malformed request, are returned at once.
    pub fn wait_controls_change(
        &mut self,
        id: &str,
        seen: u64,
        timeout: Duration,
    ) -> Result<Option<u64>, Error> {
        word(id, "camera id")?;
        let deadline = Instant::now() + timeout;
        let mut answered = false;
        let mut last_error = None;
        loop {
            match self.camera(id) {
                Ok(camera) => {
                    answered = true;
                    if let Some(seq) = camera.controls_seq.filter(|&seq| seq != seen) {
                        return Ok(Some(seq));
                    }
                }
                Err(e) if transient(e.kind) => last_error = Some(e),
                Err(e) => return Err(e),
            }
            let now = Instant::now();
            if now >= deadline {
                return match (answered, last_error) {
                    (false, Some(e)) => Err(e),
                    _ => Ok(None),
                };
            }
            std::thread::sleep(self.options.poll.min(deadline - now));
        }
    }
}

fn transient(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Unreachable | ErrorKind::Timeout | ErrorKind::Io | ErrorKind::Stopped
    )
}

/// One request on a fresh connection, all of it before `deadline`.
fn exchange(stream: UnixStream, request: &str, deadline: Instant) -> Result<Vec<String>, Error> {
    let left = |deadline: Instant| {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Timeout,
                    "the control socket did not answer in time",
                )
            })
    };
    stream
        .set_write_timeout(Some(left(deadline)?))
        .map_err(io)?;
    (&stream)
        .write_all(format!("{request}\n").as_bytes())
        .map_err(io)?;
    let mut lines = BufReader::new(stream);
    let mut answer = Vec::new();
    let mut line = Vec::new();
    loop {
        lines
            .get_ref()
            .set_read_timeout(Some(left(deadline)?))
            .map_err(io)?;
        line.clear();
        let read = (&mut lines)
            .take(MAX_ANSWER_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(io)?;
        if read == 0 {
            return Err(Error::new(ErrorKind::Stopped, "the control socket closed"));
        }
        if line.last() != Some(&b'\n') {
            return Err(Error::new(
                ErrorKind::Protocol,
                if line.len() > MAX_ANSWER_LINE {
                    "the control socket sent a line too long"
                } else {
                    "the control socket closed mid-line"
                },
            ));
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| {
                Error::new(
                    ErrorKind::Protocol,
                    "the control socket sent a line that is not UTF-8",
                )
            })?
            .trim_end_matches(['\r', '\n']);
        if text == "ok" {
            return Ok(answer);
        }
        if let Some(why) = text.strip_prefix("err") {
            return Err(Error::from_service(why.trim()));
        }
        if answer.len() >= MAX_ANSWER {
            return Err(Error::new(
                ErrorKind::Protocol,
                "the control socket sent too many lines",
            ));
        }
        answer.push(text.to_string());
    }
}

fn io(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::new(
            ErrorKind::Timeout,
            "the control socket did not answer in time",
        ),
        _ => Error::new(ErrorKind::Io, format!("control socket: {e}")),
    }
}

/// A word that goes into a request: no white space or control character, and an id no longer
/// than the service takes.
fn word<'a>(text: &'a str, what: &str) -> Result<&'a str, Error> {
    if text.is_empty()
        || text.len() > MAX_ID
        || text.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::new(
            ErrorKind::Invalid,
            format!("{what} {text:?} cannot be sent: one word of at most {MAX_ID} bytes"),
        ));
    }
    Ok(text)
}

/// The wire text of a control value, as `set` takes it. A crop is not set by text.
pub fn value_text(value: ControlValue) -> Result<String, Error> {
    match value {
        ControlValue::Auto => Ok("auto".to_string()),
        ControlValue::Manual(v) => Ok(v.to_string()),
        ControlValue::Flag(true) => Ok("on".to_string()),
        ControlValue::Flag(false) => Ok("off".to_string()),
        ControlValue::Crop(_) => Err(Error::new(
            ErrorKind::Unsupported,
            "a crop is not set through the control socket",
        )),
    }
}

/// A `controls` answer as the in-process types, with its count (0 when it has none).
pub fn info_of(answer: &ControlsAnswer) -> (u64, ControlsInfo) {
    let info = ControlsInfo {
        standard: answer.standard.iter().filter_map(standard_of).collect(),
        device: answer.device.iter().map(device_of).collect(),
    };
    (answer.seq.unwrap_or(0), info)
}

/// The value in a `controls` answer for `control`.
pub fn value_of(answer: &ControlsAnswer, control: Control) -> Result<ControlValue, Error> {
    let line = answer
        .standard
        .iter()
        .find(|line| line.name == control.name())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("the camera has no control {}", control.name()),
            )
        })?;
    match line.value {
        Shown::Value(value) => Ok(value),
        Shown::NoFrame => Err(Error::new(ErrorKind::NotFound, "no frame yet, so no value")),
        Shown::Rect => Err(Error::new(
            ErrorKind::Unsupported,
            "a crop is not read through the control socket",
        )),
    }
}

fn parse_frame(line: &str) -> Option<LastFrame> {
    let mut words = line.split_ascii_whitespace();
    if words.next()? != "frame" {
        return None;
    }
    let (mut id, mut timestamp_ns, mut exposure_us, mut gain_milli) = (None, None, None, None);
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            continue;
        };
        let number = value.parse::<u64>().ok();
        match key {
            "id" => id = number,
            "timestamp_ns" => timestamp_ns = number,
            "exposure_us" => exposure_us = number,
            "gain_milli" => gain_milli = number,
            _ => {}
        }
    }
    Some(LastFrame {
        id: id?,
        timestamp_ns: timestamp_ns?,
        exposure_us,
        gain_milli,
    })
}

fn control_of(name: &str) -> Option<Control> {
    Control::ALL.into_iter().find(|c| c.name() == name)
}

fn standard_of(line: &ControlLine) -> Option<StandardInfo> {
    Some(StandardInfo {
        control: control_of(&line.name)?,
        range: line.range,
        step: line.step,
        default: match line.default {
            Some(Shown::Value(value)) => Some(value),
            _ => None,
        },
        effective: line.effective,
        auto: line.auto,
        access: line.access,
    })
}

fn device_of(line: &DeviceLine) -> DeviceInfo {
    DeviceInfo {
        id: line.id,
        name: line.name.clone(),
        kind: line.kind,
        min: line.min,
        max: line.max,
        step: line.step,
        default: line.default,
        effective: line.effective,
        value: line.value,
        standard: line.standard.as_deref().and_then(control_of),
        access: line.access,
        menu: line.menu.clone(),
        real: line.real,
        rect: line.rect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(lines: &[&str]) -> ControlsAnswer {
        parse_controls_answer(lines)
    }

    #[test]
    fn a_value_is_read_from_its_control_line() {
        let a = answer(&[
            "control exposure unit=us range=1..9 default=10000 auto=true value=5000",
            "control gain unit=- range=1..9 default=1 auto=false value=-",
        ]);
        assert_eq!(
            value_of(&a, Control::Exposure),
            Ok(ControlValue::Manual(5000))
        );
        assert_eq!(
            value_of(&a, Control::Gain).map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(
            value_of(&a, Control::FrameRate).map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );
    }

    #[test]
    fn the_services_refusals_have_the_kinds_of_the_in_process_check() {
        use styx_select::Access;
        for (text, refused) in [
            (
                "OutOfRange { min: 1000, max: 15938, value: 16000 }",
                Refusal::OutOfRange {
                    min: 1_000,
                    max: 15_938,
                    value: 16_000,
                },
            ),
            (
                "NotStep { min: 0, step: 4, value: 3 }",
                Refusal::NotStep {
                    min: 0,
                    step: 4,
                    value: 3,
                },
            ),
            ("NoEntry { value: 9 }", Refusal::NoEntry { value: 9 }),
            (
                "the control is read_only",
                Refusal::Access(Access::ReadOnly),
            ),
            (
                "the value is the wrong kind for this control",
                Refusal::Kind,
            ),
        ] {
            assert_eq!(
                refused.to_string(),
                text,
                "the service's text is the refusal's"
            );
            assert_eq!(
                Error::from_service(text).kind,
                Error::refusal(refused).kind,
                "{text}"
            );
        }
        for (text, kind) in [
            ("the camera has no control focus", ErrorKind::NotFound),
            (
                "the camera has no device control 0xdead",
                ErrorKind::NotFound,
            ),
            ("no camera x", ErrorKind::NotFound),
            ("no frame yet", ErrorKind::NotFound),
            ("camera is absent, not streaming", ErrorKind::Stopped),
            ("unsupported by this backend: x", ErrorKind::Unsupported),
            ("something new", ErrorKind::Service),
        ] {
            assert_eq!(Error::from_service(text).kind, kind, "{text}");
        }
    }

    #[test]
    fn a_controls_answer_reads_back_into_the_in_process_types() {
        let a = answer(&[
            "seq 4",
            "control gain unit=milli range=1000..15938 step=- default=1000 effective=1000..15938 auto=false access=read_write value=-",
            "control future_control unit=- range=- value=-",
            "device 0x00000001 kind=menu access=read_write min=0 max=3 step=1 default=0 effective=0..3 value=2 standard=- entries=4 name=test_pattern",
            "entry 0x00000001 0",
            "entry 0x00000001 1 Colour Bars",
        ]);
        let (seq, info) = info_of(&a);
        assert_eq!(seq, 4);
        assert_eq!(info.standard.len(), 1, "an unknown control name is skipped");
        let gain = info.standard(Control::Gain).expect("gain");
        assert_eq!(
            gain.range,
            Some(styx_select::Span::Manual {
                min: 1_000,
                max: 15_938
            })
        );
        assert_eq!(gain.default, Some(ControlValue::Manual(1_000)));
        let pattern = info.device(1).expect("the test pattern");
        assert_eq!(
            (pattern.value, pattern.name.as_str()),
            (Some(2), "test_pattern")
        );
        assert_eq!(pattern.menu.len(), 2);
        assert_eq!(pattern.menu[0].label, None);
        assert_eq!(pattern.menu[1].label.as_deref(), Some("Colour Bars"));
    }

    #[test]
    fn a_frame_line_is_read_and_others_are_not() {
        assert_eq!(
            parse_frame("frame id=7 timestamp_ns=99 exposure_us=5000 gain_milli=2000 later=1"),
            Some(LastFrame {
                id: 7,
                timestamp_ns: 99,
                exposure_us: Some(5000),
                gain_milli: Some(2000)
            })
        );
        assert_eq!(parse_frame("frame timestamp_ns=99"), None);
        assert_eq!(parse_frame("camera x"), None);
    }

    #[test]
    fn words_that_could_add_a_request_are_refused() {
        for bad in ["", "a b", "a\nlist", "a\r", "a\tb", &"x".repeat(MAX_ID + 1)] {
            assert_eq!(
                word(bad, "camera id").map_err(|e| e.kind),
                Err(ErrorKind::Invalid),
                "{bad:?}"
            );
        }
        assert_eq!(word("lemnos:sensor0", "camera id"), Ok("lemnos:sensor0"));
        assert_eq!(word("/dev/video2", "camera id"), Ok("/dev/video2"));
    }

    #[test]
    fn options_are_raised_to_their_floors() {
        let client = ControlClient::with_options(
            "/nonexistent",
            Options {
                poll: Duration::from_millis(1),
                retry_first: Duration::from_secs(2),
                retry_cap: Duration::from_millis(10),
                ..Options::default()
            },
        );
        assert_eq!(client.options().poll, MIN_POLL);
        assert_eq!(client.options().retry_cap, Duration::from_secs(2));
    }
}
