//! The control socket: a line protocol for the camera list, the controls, and the start and stop
//! of a camera by hand.
//!
//! One request per line, at most [`MAX_LINE`] bytes, and at most [`MAX_TOKENS`] words. Each request
//! gets its answer lines, the last one `ok` or `err <why>`. Requests are untrusted: every length
//! is checked before it is used, and nothing in here panics on input.
//!
//! | Request | Answer |
//! |---|---|
//! | `list` | one `control socket=.. mode=.. group=.. gid=..` line for the control socket (its effective mode and group, see `access`), then one `camera <id> state=.. owner=.. starts=.. failures=.. total_failures=.. recoveries=.. frames=.. skipped=.. last_id=.. next_delay_ms=.. socket=.. publish=.. readers=.. published=.. publish_dropped=.. health=.. requested=.. achieved=.. achieved_meta=.. measured=.. rate_clamped=.. keep=.. buffers=.. reserve=.. reclaimed=.. reserve_cuts=.. rate_reason=.. mode=.. format=.. bits=.. mode_reason=.. mode_text=.. last_recovery=.. empty_frames=.. exposure_requested=.. exposure_applied=.. exposure_clamp=.. lemnos_skipped=.. last_error=..` line per camera, then one `copy copies=.. bytes=.. site=..` line per copy site (the service's process; `site` is free text, last), then `ok` |
//! | `controls <id>` | one `control <name> unit=.. range=.. default=.. auto=.. value=..` line per control (`value` is the last frame's), then `ok` |
//! | `frame <id>` | `frame id=.. timestamp_ns=.. exposure_us=.. gain_milli=..` for the last frame, then `ok` (`err` before any frame) |
//! | `set <id> <control> <value> <frame>` | `ok` once the backend took the update. `<value>` is a number, `auto`, `on` or `off`. The update applies from `<frame>` |
//! | `start <id>` / `stop <id>` | `ok`. Starts or stops the camera by hand |
//!
//! `mode` is the sensor mode's index (`-` when none), `format` its frame format, `bits` its bit
//! depth, `mode_reason` one word (`exact`, `rate_clamped_to_ceiling`, `rate_clamped_to_floor`,
//! `depth_lowered_for_rate`) and `mode_text` the reason in a sentence. `exposure_requested` and
//! `exposure_applied` are the exposure asked for and the exposure the sensor reports in force, in
//! microseconds (`unknown` until it reports, or when it reports none); `exposure_clamp` is the
//! sensor's clamp in a sentence when it moved the exposure to the nearest one it takes (`-` when it
//! did not). `last_error` and the other free text come
//! last on their line; `mode_text`, `exposure_clamp` and `rate_reason` are one word (spaces are `_`).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use styx_controls::{Control, ControlValue, Range, Update};
use styx_lemnos::{LayoutRefusal, SharedLayout};

use crate::service::Shared;
use crate::slot::{CameraStatus, Command, Slot};

/// The longest request line, in bytes, newline excluded.
pub const MAX_LINE: usize = 256;
/// The most words in a request.
pub const MAX_TOKENS: usize = 8;
/// The longest camera id a request may name.
pub const MAX_ID: usize = 128;
/// How long a connection may sit idle, and how long a camera may take to answer a `set`.
const IDLE: Duration = Duration::from_secs(2);
const ANSWER: Duration = Duration::from_secs(2);

/// Accepts connections until shutdown. Connections are served one at a time: the protocol is
/// small and the answers are quick.
pub(crate) fn serve(listener: &UnixListener, shared: &Arc<Shared>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = connection(stream, shared);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn connection(stream: UnixStream, shared: &Arc<Shared>) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IDLE))?;
    stream.set_write_timeout(Some(IDLE))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream.take(0));
    loop {
        reader.get_mut().set_limit(MAX_LINE as u64 + 1);
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        if line.last() != Some(&b'\n') {
            // Either the line is too long or the peer stopped mid-line. Answer and hang up.
            writeln!(writer, "err request is longer than {MAX_LINE} bytes")?;
            return Ok(());
        }
        line.pop();
        let lines = match std::str::from_utf8(&line) {
            Ok(text) => dispatch(text, shared),
            Err(_) => vec!["err request is not UTF-8".to_string()],
        };
        for answer in lines {
            writeln!(writer, "{answer}")?;
        }
        writer.flush()?;
    }
}

/// Answers one request line.
pub(crate) fn dispatch(line: &str, shared: &Arc<Shared>) -> Vec<String> {
    if line.len() > MAX_LINE {
        return vec![format!("err request is longer than {MAX_LINE} bytes")];
    }
    let words: Vec<&str> = line.split_ascii_whitespace().collect();
    if words.len() > MAX_TOKENS {
        return vec![format!("err at most {MAX_TOKENS} words")];
    }
    let Some((&verb, args)) = words.split_first() else {
        return vec!["err empty request".to_string()];
    };
    match (verb, args) {
        ("list", []) => {
            let mut out = vec![control_line(shared)];
            out.extend(
                shared
                    .slots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .values()
                    .map(|slot| camera_line(&slot.snapshot())),
            );
            out.extend(copy_lines());
            out.push("ok".to_string());
            out
        }
        ("controls", [id]) => with_slot(shared, id, controls),
        ("frame", [id]) => with_slot(shared, id, frame),
        ("start", [id]) => with_slot(shared, id, |slot| {
            slot.wake.store(true, Ordering::Release);
            let _ = slot.commands.send(Command::Start);
            vec!["ok".to_string()]
        }),
        ("stop", [id]) => with_slot(shared, id, |slot| {
            slot.wake.store(true, Ordering::Release);
            let _ = slot.commands.send(Command::Stop);
            vec!["ok".to_string()]
        }),
        ("set", [id, control, value, frame]) => {
            let Some(frame) = frame.parse::<u64>().ok() else {
                return vec!["err the frame must be a number".to_string()];
            };
            let Some(control) = control_named(control) else {
                return vec![format!("err no control named {}", clip(control))];
            };
            let Some(value) = parse_value(value) else {
                return vec!["err the value must be a number, auto, on or off".to_string()];
            };
            let update = Update {
                control,
                value,
                frame,
            };
            if let Err(e) = control.validate(value) {
                return vec![format!("err {e:?}")];
            }
            with_slot(shared, id, |slot| set(slot, update))
        }
        ("help", []) => vec![
            "list | controls <id> | frame <id> | set <id> <control> <value> <frame> | start <id> | stop <id>"
                .to_string(),
            "ok".to_string(),
        ],
        _ => vec![format!("err unknown request {}", clip(verb))],
    }
}

fn with_slot(shared: &Arc<Shared>, id: &str, f: impl FnOnce(&Slot) -> Vec<String>) -> Vec<String> {
    if id.len() > MAX_ID {
        return vec![format!("err a camera id is at most {MAX_ID} bytes")];
    }
    let slot = shared
        .slots
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(id)
        .cloned();
    match slot {
        Some(slot) => f(&slot),
        None => vec![format!("err no camera {}", clip(id))],
    }
}

fn set(slot: &Slot, update: Update) -> Vec<String> {
    let (tx, rx) = mpsc::channel();
    if slot
        .commands
        .send(Command::Set {
            updates: vec![update],
            reply: tx,
        })
        .is_err()
    {
        return vec!["err the camera's worker has ended".to_string()];
    }
    match rx.recv_timeout(ANSWER) {
        Ok(Ok(())) => vec!["ok".to_string()],
        Ok(Err(e)) => vec![format!("err {}", one_line(&e))],
        Err(_) => vec!["err the camera did not answer".to_string()],
    }
}

fn controls(slot: &Slot) -> Vec<String> {
    let current = slot.current_controls();
    let mut out = Vec::with_capacity(Control::COUNT + 1);
    for control in Control::ALL {
        let spec = control.spec();
        let value = match current {
            Some(values) => show(values[control.index()]),
            None => "-".to_string(),
        };
        out.push(format!(
            "control {} unit={} range={} default={} auto={} value={}",
            control.name(),
            if spec.unit.is_empty() { "-" } else { spec.unit },
            range(spec.range),
            show(spec.default),
            spec.auto,
            value,
        ));
    }
    out.push("ok".to_string());
    out
}

fn frame(slot: &Slot) -> Vec<String> {
    match slot.snapshot().last_frame {
        Some(f) => vec![
            format!(
                "frame id={} timestamp_ns={} exposure_us={} gain_milli={}",
                f.id, f.timestamp_ns, f.exposure_us, f.gain_milli
            ),
            "ok".to_string(),
        ],
        None => vec!["err no frame yet".to_string()],
    }
}

/// The control socket's effective mode and group. The group is its name when `/etc/group` has one.
fn control_line(shared: &Shared) -> String {
    format!(
        "control socket={} mode={:04o} group={} gid={}",
        clip(&shared.control_socket.display().to_string()),
        shared.access.socket_mode,
        shared.access.group,
        shared.access.gid,
    )
}

/// The copy counters of this process: every site where a frame's bytes are copied.
fn copy_lines() -> Vec<String> {
    styx_frame::copy_counters()
        .iter()
        .copied()
        .chain(styx_v4l2::copy_counters().iter().copied())
        .map(|c| {
            // The site is last, as free text: it may hold spaces.
            format!(
                "copy copies={} bytes={} site={}",
                c.copies(),
                c.bytes(),
                c.site()
            )
        })
        .collect()
}

fn camera_line(status: &CameraStatus) -> String {
    let owner = status
        .owner
        .map_or_else(|| "-".to_string(), |b| b.to_string());
    let milli = |v: Option<u32>| v.map_or_else(|| "none".to_string(), |v| v.to_string());
    let requested = milli(status.requested_milli);
    let achieved = status
        .achieved_milli
        .map_or_else(|| "unknown".to_string(), |v| v.to_string());
    let reason = status
        .rate_reason()
        .map_or_else(|| "-".to_string(), |r| r.replace(' ', "_"));
    let keep = status.keep;
    let last_id = status
        .last_frame
        .map_or_else(|| "-".to_string(), |f| f.id.to_string());
    let delay = status
        .next_delay
        .map_or_else(|| "-".to_string(), |d| d.as_millis().to_string());
    let recovery = status
        .last_recovery
        .as_deref()
        .map_or_else(|| "-".to_string(), |r| r.replace(' ', "_"));
    let error = status
        .last_error
        .as_deref()
        .map_or_else(|| "-".to_string(), one_line);
    // The producer's state: `running`, or `retrying:<since_ns>` (CLOCK_MONOTONIC nanoseconds).
    let producer = status.producer_since_ns.map_or_else(
        || "running".to_string(),
        |since| format!("retrying:{since}"),
    );
    // The layout refusal as `<ring|marks>:<found>:<expected>`, and the fault code number.
    let layout_refused = status
        .layout_refused
        .map_or_else(|| "-".to_string(), |r| format_layout(&r));
    let fault = status
        .fault
        .map_or_else(|| "-".to_string(), |f| f.to_string());
    let socket = status
        .socket
        .as_ref()
        .map_or_else(|| "-".to_string(), |p| p.display().to_string());
    // Free text in this line is one word (spaces become `_`), except `last_error`, which is last.
    let publish = status.publish.to_string().replace(' ', "_");
    let (exposure_requested, exposure_applied, exposure_clamp) = match &status.exposure {
        Some(exposure) => (
            exposure.requested_us.to_string(),
            exposure
                .applied_us
                .map_or_else(|| "unknown".to_string(), |us| us.to_string()),
            exposure
                .clamp
                .as_ref()
                .map_or_else(|| "-".to_string(), |c| one_line(&c.text).replace(' ', "_")),
        ),
        None => ("-".to_string(), "-".to_string(), "-".to_string()),
    };
    let (mode, format, bits, mode_reason, mode_text) = match &status.mode {
        Some(chosen) => (
            chosen.mode.to_string(),
            chosen.format.to_string(),
            chosen.bit_depth.to_string(),
            chosen.reason.name().to_string(),
            one_line(&chosen.text).replace(' ', "_"),
        ),
        None => (
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
        ),
    };
    format!(
        "camera {} state={} owner={} starts={} failures={} total_failures={} recoveries={} frames={} skipped={} last_id={} next_delay_ms={} socket={} publish={} readers={} published={} publish_dropped={} health={} requested={} achieved={} achieved_meta={} measured={} rate_clamped={} keep={} buffers={} reserve={} reclaimed={} reserve_cuts={} rate_reason={} mode={} format={} bits={} mode_reason={} mode_text={} last_recovery={} empty_frames={} exposure_requested={} exposure_applied={} exposure_clamp={} lemnos_skipped={} producer={} layout_refused={} fault={} last_error={}",
        clip(&status.id),
        status.state,
        owner,
        status.starts,
        status.failures,
        status.total_failures,
        status.recoveries,
        status.frames,
        status.skipped,
        last_id,
        delay,
        socket,
        publish,
        status.readers,
        status.published,
        status.publish_dropped,
        status.health.to_string().replace(' ', "_"),
        requested,
        achieved,
        milli(status.achieved_meta_milli),
        milli(status.measured_milli),
        status.rate_clamped,
        keep.keep,
        keep.buffers,
        keep.hardware_reserve,
        keep.reclaimed,
        keep.reserve_cuts,
        reason,
        mode,
        format,
        bits,
        mode_reason,
        mode_text,
        recovery,
        status.empty_frames,
        exposure_requested,
        exposure_applied,
        exposure_clamp,
        status.lemnos_skipped,
        producer,
        layout_refused,
        fault,
        error,
    )
}

// Reading the answers: the client side of this protocol, next to the formatting above. A client
// (the C surface, the measure tool) uses these instead of splitting the lines itself. Unknown line
// kinds and unknown fields are skipped, so a later addition to the service does not break an older
// client. A field the line does not carry, or carries as `-`, is `None`.

/// One `camera <id> key=value ...` line of a `list` answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CameraLine {
    pub id: String,
    /// The camera's state word (`streaming`, `backoff`, ...).
    pub state: Option<String>,
    /// The owning backend (`native`, `libcamera`, `v4l2`), `None` before a scan lists it.
    pub owner: Option<String>,
    /// The camera's frame socket, when it is publishing.
    pub socket: Option<PathBuf>,
    pub frames: Option<u64>,
    pub last_id: Option<u64>,
    /// The publish state as one word (`waiting`, `publishing`, `not-publishable_(why)`).
    pub publish: Option<String>,
    /// The health as one word, as the service writes it (spaces are `_`).
    pub health: Option<String>,
    pub readers: Option<u64>,
    pub published: Option<u64>,
    /// The rate asked for, in milli-fps (`None` when nothing asks for one).
    pub requested_milli: Option<u32>,
    /// The backend's reported rate, in milli-fps (`None` when unknown).
    pub achieved_milli: Option<u32>,
    /// The achieved rate in the last frame's metadata, in milli-fps.
    pub achieved_meta_milli: Option<u32>,
    /// The rate measured from the capture timestamps, in milli-fps.
    pub measured_milli: Option<u32>,
    pub rate_clamped: Option<bool>,
    /// Why the rates differ, as the service writes it (one word: spaces are `_`).
    pub rate_reason: Option<String>,
    /// The producer's state as one word: `running`, or `retrying:<since_ns>`.
    pub producer: Option<String>,
    /// The layout refusal in force, as its values (`layout_refused=<ring|marks>:<found>:<expected>`).
    pub layout_refused: Option<LayoutRefusal>,
    /// The fault code of the camera's last failure (the Daedalus fault table's number).
    pub fault: Option<u32>,
    /// The last failure's message: the free text at the end of the line.
    pub last_error: Option<String>,
}

/// One `copy copies=.. bytes=.. site=..` line of a `list` answer: a site where frame bytes were
/// copied, in the service's process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyLine {
    pub copies: u64,
    pub bytes: u64,
    /// The site's name (free text: it may hold spaces).
    pub site: String,
}

/// The lines of a `list` answer that a client reads. Anything else (the `ok`, an `err`, a kind a
/// later service adds) is skipped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    pub cameras: Vec<CameraLine>,
    pub copies: Vec<CopyLine>,
}

/// Reads a `list` answer (its lines, with or without the closing `ok`).
pub fn parse_list<S: AsRef<str>>(lines: &[S]) -> Listing {
    let mut listing = Listing::default();
    for line in lines {
        let line = line.as_ref();
        if line.starts_with("camera ") {
            listing.cameras.extend(parse_camera_line(line));
        } else if line.starts_with("copy ") {
            listing.copies.extend(parse_copy_line(line));
        }
    }
    listing
}

/// One camera line. The free text `last_error` is everything after its key.
pub fn parse_camera_line(line: &str) -> Option<CameraLine> {
    let (head, last_error) = match line.find(" last_error=") {
        Some(at) => (&line[..at], Some(&line[at + " last_error=".len()..])),
        None => (line, None),
    };
    let mut words = head.split_ascii_whitespace();
    if words.next()? != "camera" {
        return None;
    }
    let mut camera = CameraLine {
        id: words.next()?.to_string(),
        ..CameraLine::default()
    };
    let mut seen = Vec::new();
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            continue;
        };
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let dash = value == "-";
        let text = (!dash).then(|| value.to_string());
        let number = value.parse::<u64>().ok();
        let milli = value.parse::<u32>().ok();
        match key {
            "state" => camera.state = text,
            "owner" => camera.owner = text,
            "socket" => camera.socket = text.map(PathBuf::from),
            "frames" => camera.frames = number,
            "last_id" => camera.last_id = number,
            "publish" => camera.publish = text,
            "health" => camera.health = text,
            "readers" => camera.readers = number,
            "published" => camera.published = number,
            "requested" => camera.requested_milli = milli,
            "achieved" => camera.achieved_milli = milli,
            "achieved_meta" => camera.achieved_meta_milli = milli,
            "measured" => camera.measured_milli = milli,
            "rate_clamped" => camera.rate_clamped = value.parse::<bool>().ok(),
            "rate_reason" => camera.rate_reason = text,
            "producer" => camera.producer = text,
            "layout_refused" => camera.layout_refused = parse_layout(value),
            "fault" => camera.fault = value.parse::<u32>().ok(),
            _ => {}
        }
    }
    camera.last_error = last_error.filter(|text| *text != "-").map(str::to_string);
    Some(camera)
}

/// The layout refusal as the line writes it: `ring` or `marks`, then the found and the expected
/// values in decimal, separated by `:`.
fn format_layout(refusal: &LayoutRefusal) -> String {
    let which = match refusal.which {
        SharedLayout::Ring => "ring",
        SharedLayout::Marks => "marks",
    };
    format!("{which}:{}:{}", refusal.found, refusal.expected)
}

/// Reads [`format_layout`]'s text; `-` and anything else is `None`.
fn parse_layout(value: &str) -> Option<LayoutRefusal> {
    let mut parts = value.split(':');
    let which = match parts.next()? {
        "ring" => SharedLayout::Ring,
        "marks" => SharedLayout::Marks,
        _ => return None,
    };
    let found = parts.next()?.parse().ok()?;
    let expected = parts.next()?.parse().ok()?;
    Some(LayoutRefusal {
        which,
        found,
        expected,
    })
}

/// One copy line: `copy copies=.. bytes=.. site=<free text>`.
pub fn parse_copy_line(line: &str) -> Option<CopyLine> {
    let rest = line.strip_prefix("copy ")?;
    let (head, site) = rest.split_once(" site=")?;
    let (mut copies, mut bytes) = (None, None);
    for word in head.split_ascii_whitespace() {
        match word.split_once('=') {
            Some(("copies", v)) => copies = v.parse().ok(),
            Some(("bytes", v)) => bytes = v.parse().ok(),
            _ => {}
        }
    }
    Some(CopyLine {
        copies: copies?,
        bytes: bytes?,
        site: site.to_string(),
    })
}

/// One control's line of a `controls` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlLine {
    /// The control's name, as `Control::name` gives it.
    pub name: String,
    pub value: Shown,
}

/// A control's value as the `controls` answer shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    /// `-`: no frame has arrived, so there is no value yet.
    NoFrame,
    /// `rect`: a crop, which the answer does not give a number for.
    Rect,
    Value(ControlValue),
}

/// Reads a `controls` answer (its lines, with or without the closing `ok`).
pub fn parse_controls<S: AsRef<str>>(lines: &[S]) -> Vec<ControlLine> {
    lines
        .iter()
        .filter_map(|line| {
            let mut words = line.as_ref().split_ascii_whitespace();
            if words.next()? != "control" {
                return None;
            }
            let name = words.next()?.to_string();
            let value = words
                .find_map(|word| word.strip_prefix("value="))
                .and_then(parse_shown)?;
            Some(ControlLine { name, value })
        })
        .collect()
}

/// The `value=` word of a `controls` line, as `show` writes it.
fn parse_shown(text: &str) -> Option<Shown> {
    Some(match text {
        "-" => Shown::NoFrame,
        "rect" => Shown::Rect,
        "auto" => Shown::Value(ControlValue::Auto),
        "on" => Shown::Value(ControlValue::Flag(true)),
        "off" => Shown::Value(ControlValue::Flag(false)),
        number => Shown::Value(ControlValue::Manual(number.parse().ok()?)),
    })
}

fn control_named(name: &str) -> Option<Control> {
    Control::ALL.into_iter().find(|c| c.name() == name)
}

fn parse_value(text: &str) -> Option<ControlValue> {
    match text {
        "auto" => Some(ControlValue::Auto),
        "on" => Some(ControlValue::Flag(true)),
        "off" => Some(ControlValue::Flag(false)),
        number => number.parse::<u32>().ok().map(ControlValue::Manual),
    }
}

fn show(value: ControlValue) -> String {
    match value {
        ControlValue::Auto => "auto".to_string(),
        ControlValue::Manual(v) => v.to_string(),
        ControlValue::Flag(true) => "on".to_string(),
        ControlValue::Flag(false) => "off".to_string(),
        ControlValue::Crop(_) => "rect".to_string(),
    }
}

fn range(range: Range) -> String {
    match range {
        Range::Manual { min, max } => format!("{min}..{max}"),
        Range::Flag => "flag".to_string(),
        Range::Rect => "rect".to_string(),
    }
}

/// Free text as one line, cut to a sane length.
fn one_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    clip(&flat)
}

fn clip(text: &str) -> String {
    const LIMIT: usize = 200;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CameraConfig;
    use crate::slot::Slot;

    fn slot(id: &str) -> Slot {
        let (tx, _rx) = mpsc::channel();
        let settings = CameraConfig {
            fps_milli: Some(60_000),
            ..CameraConfig::default()
        };
        Slot::new(id.to_string(), settings, tx)
    }

    #[test]
    fn a_camera_line_the_service_writes_reads_back_field_for_field() {
        let slot = slot("lemnos:cam0");
        let line = camera_line(&slot.snapshot());
        let cam = parse_camera_line(&line).expect("a camera line");
        assert_eq!(cam.id, "lemnos:cam0");
        assert_eq!(cam.state.as_deref(), Some("absent"));
        assert_eq!(cam.owner, None, "no owner before a scan lists the camera");
        assert_eq!(cam.socket, None);
        assert_eq!(cam.frames, Some(0));
        assert_eq!(cam.requested_milli, Some(60_000));
        assert_eq!(cam.achieved_milli, None, "`unknown` reads as absent");
        assert_eq!(cam.measured_milli, None, "`none` reads as absent");
        assert_eq!(cam.last_error, None);
    }

    #[test]
    fn a_layout_refusal_injected_at_the_status_shows_as_fields_before_the_text() {
        // The status a native session reports when its frame stream refuses the other side's
        // layout: the refusal's values and the fault code, then the text of the failure.
        let slot = slot("lemnos:cam0");
        let refusal = LayoutRefusal {
            which: SharedLayout::Marks,
            found: 1,
            expected: 2,
        };
        slot.layout_refused(Some(refusal));
        slot.retrying(
            "frame stream cam0: the marks page is layout 1; this build reads layout 2".into(),
        );
        let line = camera_line(&slot.snapshot());
        assert!(
            line.contains(
                "layout_refused=marks:1:2 fault=1 last_error=frame stream cam0: the marks page"
            ),
            "the fields come before the free text: {line}"
        );
        let cam = parse_camera_line(&line).expect("a camera line");
        assert_eq!(
            cam.layout_refused,
            Some(LayoutRefusal {
                which: SharedLayout::Marks,
                found: 1,
                expected: 2,
            })
        );
        assert_eq!(cam.fault, Some(1));
        assert!(
            cam.last_error
                .as_deref()
                .is_some_and(|e| e.starts_with("frame stream cam0"))
        );
        // A refusal that ends clears its fields; the line says so with `-`.
        slot.layout_refused(None);
        let cam = parse_camera_line(&camera_line(&slot.snapshot())).expect("a camera line");
        assert_eq!(cam.layout_refused, None);
        assert_eq!(cam.fault, None);
    }

    #[test]
    fn free_text_at_the_end_keeps_its_spaces_and_a_socket_is_read() {
        let mut status = slot("cam").snapshot();
        status.socket = Some(PathBuf::from("/run/s/cam-1.sock"));
        status.state = crate::slot::State::Streaming;
        status.last_error = Some("the camera failed: no link at all".to_string());
        status.achieved_milli = Some(119_990);
        status.achieved_meta_milli = Some(119_988);
        status.rate_clamped = true;
        let line = camera_line(&status);
        let cam = parse_camera_line(&line).expect("a camera line");
        assert_eq!(cam.socket, Some(PathBuf::from("/run/s/cam-1.sock")));
        assert_eq!(cam.state.as_deref(), Some("streaming"));
        assert_eq!(cam.achieved_milli, Some(119_990));
        assert_eq!(cam.achieved_meta_milli, Some(119_988));
        assert_eq!(cam.rate_clamped, Some(true));
        assert!(
            cam.rate_reason.is_some(),
            "the rates differ, so there is a reason"
        );
        assert_eq!(
            cam.last_error.as_deref(),
            Some("the camera failed: no link at all")
        );
    }

    #[test]
    fn unknown_kinds_and_fields_are_skipped_not_errors() {
        let lines = [
            "camera a state=streaming some_future_field=7 owner=native",
            "gizmo 1 2 3",
            "copy copies=4 bytes=64 site=a site with spaces",
            "copy copies=x bytes=1 site=broken",
            "ok",
        ];
        let listing = parse_list(&lines);
        assert_eq!(listing.cameras.len(), 1);
        assert_eq!(listing.cameras[0].owner.as_deref(), Some("native"));
        assert_eq!(
            listing.copies,
            vec![CopyLine {
                copies: 4,
                bytes: 64,
                site: "a site with spaces".to_string()
            }]
        );
    }

    #[test]
    fn the_copy_lines_the_service_writes_read_back() {
        let lines = copy_lines();
        let listing = parse_list(&lines);
        assert_eq!(listing.copies.len(), lines.len());
        for (line, copy) in lines.iter().zip(&listing.copies) {
            assert!(line.contains(&copy.site), "{line}");
        }
    }

    #[test]
    fn a_control_value_round_trips_through_its_shown_form() {
        for value in [
            ControlValue::Auto,
            ControlValue::Manual(5000),
            ControlValue::Flag(true),
            ControlValue::Flag(false),
        ] {
            assert_eq!(parse_shown(&show(value)), Some(Shown::Value(value)));
        }
        assert_eq!(parse_shown("-"), Some(Shown::NoFrame));
        assert_eq!(parse_shown("rect"), Some(Shown::Rect));
        assert_eq!(parse_shown("banana"), None);
    }

    #[test]
    fn the_controls_answer_the_service_writes_reads_back_by_name() {
        let slot = slot("cam");
        let mut lines = controls(&slot);
        lines.push("ok".to_string());
        let parsed = parse_controls(&lines);
        assert_eq!(parsed.len(), Control::COUNT);
        assert!(parsed.iter().all(|c| c.value == Shown::NoFrame));
        assert!(parsed.iter().any(|c| c.name == "exposure"));
    }
}
