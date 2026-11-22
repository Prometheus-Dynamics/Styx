//! The control socket: a line protocol for the camera list, the controls, and the start and stop
//! of a camera by hand.
//!
//! One request per line, at most [`MAX_LINE`] bytes, and at most [`MAX_TOKENS`] words. Each request
//! gets its answer lines, the last one `ok` or `err <why>`. Requests are untrusted: every length
//! is checked before it is used, and nothing in here panics on input.
//!
//! | Request | Answer |
//! |---|---|
//! | `list` | one `control socket=.. mode=.. group=.. gid=..` line for the control socket (its effective mode and group, see `access`), then one `camera <id> state=.. owner=.. starts=.. failures=.. total_failures=.. recoveries=.. frames=.. skipped=.. last_id=.. next_delay_ms=.. socket=.. publish=.. readers=.. published=.. publish_dropped=.. health=.. requested=.. achieved=.. achieved_meta=.. measured=.. rate_clamped=.. keep=.. kept=.. buffers=.. reserve=.. reclaimed=.. reserve_cuts=.. rate_reason=.. mode=.. format=.. bits=.. mode_reason=.. mode_text=.. last_recovery=.. empty_frames=.. exposure_requested=.. exposure_applied=.. exposure_clamp=.. lemnos_skipped=.. lemnos_at_limit=.. producer=.. layout_refused=.. fault=.. bound_unmet=.. discontinuities=.. contract_mismatch=.. controls_seq=.. isp=.. isp_asked=.. isp_form=.. isp_refused=.. isp_skipped=.. isp_switches=<to_soft>/<to_pisp> isp_published=a/b isp_dropped=a/b isp_readers=a/b isp0_socket=.. isp1_socket=.. isp_3a=.. isp_reason_at=.. isp_reason=.. last_error=..` line per camera, then one `copy copies=.. bytes=.. [bytes_read=..] site=..` line per copy site (the service's process; `bytes` is what was written, `bytes_read` is given where the copy changes the form; `site` is free text, last), then `ok` |
//! | `controls <id>` | `seq <n>` (the camera's `controls_seq`), one `control <name> unit=.. range=.. step=.. default=.. effective=.. auto=.. access=.. value=..` line per standard control the camera has, one `device <id> kind=.. access=.. min=.. max=.. step=.. default=.. effective=.. value=.. standard=.. entries=.. name=..` line per device control (`name` is free text, last), each followed by its menu's `entry <id> <value> [<label>]` lines, then `ok` |
//! | `getd <id> <device-id>` | that device control's `device` line and its `entry` lines, then `ok` |
//! | `setd <id> <device-id> <value>` | `ok` once the backend took the value (the device's own units) |
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
//!
//! Controls are the camera's own (`styx_select::ControlsInfo`), never a generic table:
//!
//! - A `control` line is a standard control the camera has, in its documented styx-controls unit
//!   (`unit=`: exposure `us`, gain `milli`, white balance `K`, frame rate `mHz`, ...) whatever unit the
//!   device uses: a backend converts (V4L2's exposure in 100 us steps is shown in microseconds). A
//!   standard control the camera does not have has no line. `range` is `<min>..<max>`, `flag` or
//!   `rect`; `effective` is the range it takes now (exposure under the frame length in force). What
//!   the backend cannot say is `unknown`: no range is made up. `value` is the last frame's (`-`
//!   before a frame).
//! - A `device` line is one control the camera has, in the device's own units, by the backend's own
//!   id (`0x` hex): a V4L2 `V4L2_CID_*`, a libcamera control id, or for a native sensor
//!   `styx_select::NATIVE_TEST_PATTERN`. The id is stable for a camera and per backend;
//!   `standard=<name>` (or `-`) names the standard control it is, the portable handle. Setting either
//!   moves the same value. `kind` is `integer`, `integer64`, `boolean`, `menu`, `bitmask`, `button`,
//!   `float`, `rect` or `other`; `entries` is the count of `entry` lines that follow. An entry without
//!   a label is numbered only (the device names none).
//! - A `float` line's `min`, `max`, `default`, `effective` and `value` are reals, written as the
//!   shortest text that reads back as the same `f32` and always with a `.` or an exponent (`-1.0`,
//!   `0.5`, `1e-7`). They are `f32` as the camera gives them (libcamera): no more precision is
//!   claimed. A bound the camera gives as infinity or NaN is `unknown`. A `rect` line's are
//!   rectangles, `x,y,WxH` (`0,0,4056x3040`). Every other kind's are integers.
//! - `access` is `read_write`, `read_only`, `inactive`, `grabbed` or `disabled`. A set of a
//!   read-only, grabbed or disabled control is refused with that word; an inactive one is shown
//!   greyed and still taken.
//! - `setd` on a `float` control takes a real in the same form; a value without a `.` or an exponent
//!   (an integer), NaN or an infinity is `err the value is the wrong kind for this control`, so an
//!   integer is never taken for a real by accident. A `rect` control is not set by id (crop has its
//!   standard control).
//! - `set` and `setd` check only what the camera says: a value outside its range (`min..max`) is
//!   `err OutOfRange { min: .., max: .., value: .. }` with the camera's numbers, off its step
//!   `err NotStep { .. }`, a menu value with no entry `err NoEntry { .. }`. A value inside the range
//!   but past the effective one is taken: the camera clamps it and the status says why (for exposure,
//!   `exposure_clamp`). A bound the camera does not give is not checked here: the backend clamps, and
//!   the next `controls` shows what it applied.
//! - `controls_seq=` on the camera's `list` line moves on every change: one move for each value any
//!   client sets (by either name), a range that moved, the camera opened again.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use styx_controls::{Control, ControlValue, DeviceKind, Rect, Update};
use styx_lemnos::{LayoutRefusal, SharedLayout};
use styx_select::{
    Access, DeviceInfo, MenuEntry, Real, RealValues, RectValues, Refusal, Span, StandardInfo,
    parse_real, real_text,
};

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
        ("getd", [id, device]) => {
            let Some(device) = parse_device_id(device) else {
                return vec!["err a device control id is a number (0x for hex)".to_string()];
            };
            with_slot(shared, id, |slot| {
                let (info, _) = slot.controls_info();
                match info.device(device) {
                    Some(control) => {
                        let mut out = device_lines(control);
                        out.push("ok".to_string());
                        out
                    }
                    None => vec![format!("err the camera has no device control {device:#x}")],
                }
            })
        }
        ("setd", [id, device, value]) => {
            let Some(device) = parse_device_id(device) else {
                return vec!["err a device control id is a number (0x for hex)".to_string()];
            };
            with_slot(shared, id, |slot| {
                let (info, _) = slot.controls_info();
                let Some(control) = info.device(device) else {
                    return vec![format!("err the camera has no device control {device:#x}")];
                };
                match device_set(control, value) {
                    Ok(DeviceSet::Integer(value)) => ask_worker(slot, |reply| Command::SetDevice {
                        id: device,
                        value,
                        reply,
                    }),
                    Ok(DeviceSet::Real(value)) => {
                        ask_worker(slot, |reply| Command::SetDeviceReal {
                            id: device,
                            value,
                            reply,
                        })
                    }
                    Err(why) => vec![format!("err {why}")],
                }
            })
        }
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
            with_slot(shared, id, |slot| {
                let (info, _) = slot.controls_info();
                let Some(standard) = info.standard(control) else {
                    return vec![format!("err the camera has no control {}", control.name())];
                };
                if let Err(refusal) = standard.check(value) {
                    return vec![format!("err {refusal}")];
                }
                set(slot, update)
            })
        }
        ("help", []) => vec![
            "list | controls <id> | frame <id> | set <id> <control> <value> <frame> | getd <id> <device-id> | setd <id> <device-id> <value> | start <id> | stop <id>"
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
    ask_worker(slot, |reply| Command::Set {
        updates: vec![update],
        reply,
    })
}

/// Sends a command that answers, and waits for the answer.
fn ask_worker(
    slot: &Slot,
    command: impl FnOnce(mpsc::Sender<Result<(), String>>) -> Command,
) -> Vec<String> {
    let (tx, rx) = mpsc::channel();
    if slot.commands.send(command(tx)).is_err() {
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
    let (info, seq) = slot.controls_info();
    let mut out = Vec::with_capacity(info.standard.len() + info.device.len() + 2);
    out.push(format!("seq {seq}"));
    for standard in &info.standard {
        let value = match current {
            Some(values) => show(values[standard.control.index()]),
            None => "-".to_string(),
        };
        out.push(standard_line(standard, &value));
    }
    for device in &info.device {
        out.extend(device_lines(device));
    }
    out.push("ok".to_string());
    out
}

fn unknown<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "unknown".to_string(), |v| v.to_string())
}

fn standard_line(info: &StandardInfo, value: &str) -> String {
    let unit = info.control.spec().unit;
    format!(
        "control {} unit={} range={} step={} default={} effective={} auto={} access={} value={}",
        info.control.name(),
        if unit.is_empty() { "-" } else { unit },
        info.range.map_or_else(|| "unknown".to_string(), span),
        unknown(info.step),
        info.default.map_or_else(|| "unknown".to_string(), show),
        info.effective
            .map_or_else(|| "unknown".to_string(), |(lo, hi)| format!("{lo}..{hi}")),
        unknown(info.auto),
        info.access.name(),
        value,
    )
}

/// A rectangle as a device line writes it: `x,y,WxH`.
fn rect_text(rect: Rect) -> String {
    format!("{},{},{}x{}", rect.x, rect.y, rect.width, rect.height)
}

fn parse_rect(text: &str) -> Option<Rect> {
    let (x, rest) = text.split_once(',')?;
    let (y, size) = rest.split_once(',')?;
    let (width, height) = size.split_once('x')?;
    Some(Rect {
        x: x.parse().ok()?,
        y: y.parse().ok()?,
        width: width.parse().ok()?,
        height: height.parse().ok()?,
    })
}

/// A device line's min, max, default, effective and value as text, in the control's own form: an
/// integer, a real (`real_text`), or a rectangle (`rect_text`).
fn device_numbers(info: &DeviceInfo) -> [String; 5] {
    let text = |v: Option<String>| v.unwrap_or_else(|| "unknown".to_string());
    if let Some(real) = info.real {
        let r = |v: Option<Real>| text(v.map(|v| real_text(v.0)));
        return [
            r(real.min),
            r(real.max),
            r(real.default),
            text(
                real.effective
                    .map(|(lo, hi)| format!("{}..{}", real_text(lo.0), real_text(hi.0))),
            ),
            r(real.value),
        ];
    }
    if let Some(rect) = info.rect {
        let r = |v: Option<Rect>| text(v.map(rect_text));
        return [
            r(rect.min),
            r(rect.max),
            r(rect.default),
            "unknown".to_string(),
            r(rect.value),
        ];
    }
    [
        unknown(info.min),
        unknown(info.max),
        unknown(info.default),
        unknown(info.effective.map(|(lo, hi)| format!("{lo}..{hi}"))),
        unknown(info.value),
    ]
}

/// A device control's line and its menu's entry lines.
fn device_lines(info: &DeviceInfo) -> Vec<String> {
    let mut out = Vec::with_capacity(1 + info.menu.len());
    let [min, max, default, effective, value] = device_numbers(info);
    out.push(format!(
        "device {:#010x} kind={} access={} min={} max={} step={} default={} effective={} value={} standard={} entries={} name={}",
        info.id,
        kind_name(info.kind),
        info.access.name(),
        min,
        max,
        unknown(info.step),
        default,
        effective,
        value,
        info.standard.map_or("-", Control::name),
        info.menu.len(),
        one_line(&info.name),
    ));
    for entry in &info.menu {
        out.push(match &entry.label {
            Some(label) => format!(
                "entry {:#010x} {} {}",
                info.id,
                entry.value,
                one_line(label)
            ),
            None => format!("entry {:#010x} {}", info.id, entry.value),
        });
    }
    out
}

const KINDS: [(DeviceKind, &str); 9] = [
    (DeviceKind::Integer, "integer"),
    (DeviceKind::Integer64, "integer64"),
    (DeviceKind::Boolean, "boolean"),
    (DeviceKind::Menu, "menu"),
    (DeviceKind::Bitmask, "bitmask"),
    (DeviceKind::Button, "button"),
    (DeviceKind::Other, "other"),
    (DeviceKind::Float, "float"),
    (DeviceKind::Rect, "rect"),
];

fn kind_name(kind: DeviceKind) -> &'static str {
    KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or("other", |(_, name)| name)
}

fn kind_named(name: &str) -> Option<DeviceKind> {
    KINDS.iter().find(|(_, n)| *n == name).map(|(k, _)| *k)
}

/// A `setd` value as the control takes it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DeviceSet {
    Integer(i64),
    Real(f32),
}

/// Reads and checks a `setd` value against what the camera says of `control`: a real for a
/// `float` control (a decimal with a `.` or an exponent; an integer is the wrong kind, as are NaN
/// and the infinities), an integer for the others. The error is the `err` text.
fn device_set(control: &DeviceInfo, text: &str) -> Result<DeviceSet, String> {
    if control.kind == DeviceKind::Float {
        if control.access.refuses_set() {
            return Err(Refusal::Access(control.access).to_string());
        }
        let value = parse_real(text).ok_or_else(|| Refusal::Kind.to_string())?;
        control
            .real
            .unwrap_or_default()
            .check(value)
            .map_err(|r| r.to_string())?;
        return Ok(DeviceSet::Real(value));
    }
    let value = text
        .parse::<i64>()
        .map_err(|_| "the value must be a number".to_string())?;
    control.check(value).map_err(|r| r.to_string())?;
    Ok(DeviceSet::Integer(value))
}

/// A device control id: decimal, or hex with `0x`.
fn parse_device_id(text: &str) -> Option<u32> {
    match text.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
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
        .chain(std::iter::once({
            // The back end's input pass reads one form and writes another: both are given
            // (`bytes` is what was written into the input buffers).
            let input = &styx_pisp::PISP_BE_INPUT;
            format!(
                "copy copies={} bytes={} bytes_read={} site={}",
                input.passes(),
                input.bytes_written(),
                input.bytes_read(),
                input.site()
            )
        }))
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
    // The contract another build's that refuses the camera, by name (one word; `-` when none).
    let contract_mismatch = status
        .contract_mismatch
        .as_ref()
        .map_or_else(|| "-".to_string(), |c| c.replace(' ', "_"));
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
    let isp = &status.isp;
    let isp_socket = |k: usize| {
        isp.sockets[k]
            .as_ref()
            .map_or_else(|| "-".to_string(), |p| p.display().to_string())
    };
    let isp_reason = isp
        .reason
        .as_deref()
        .map_or_else(|| "-".to_string(), |r| one_line(r).replace(' ', "_"));
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
        "camera {} state={} owner={} starts={} failures={} total_failures={} recoveries={} frames={} skipped={} last_id={} next_delay_ms={} socket={} publish={} readers={} published={} publish_dropped={} health={} requested={} achieved={} achieved_meta={} measured={} rate_clamped={} keep={} kept={} buffers={} reserve={} reclaimed={} reserve_cuts={} rate_reason={} mode={} format={} bits={} mode_reason={} mode_text={} last_recovery={} empty_frames={} exposure_requested={} exposure_applied={} exposure_clamp={} lemnos_skipped={} lemnos_at_limit={} producer={} layout_refused={} fault={} bound_unmet={} discontinuities={} contract_mismatch={} controls_seq={} isp={} isp_asked={} isp_form={} isp_refused={} isp_skipped={} isp_switches={} isp_published={} isp_dropped={} isp_readers={} isp0_socket={} isp1_socket={} isp_3a={} isp_reason_at={} isp_reason={} last_error={}",
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
        keep.kept,
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
        status.lemnos_at_limit,
        producer,
        layout_refused,
        fault,
        keep.bound_unmet,
        status.discontinuities,
        contract_mismatch,
        status.controls_seq,
        isp.active.map_or("none", |a| a.name()),
        isp.asked.name(),
        isp.input_form.unwrap_or("-"),
        isp.refused,
        isp.skipped,
        format_args!("{}/{}", isp.to_soft, isp.to_pisp),
        format_args!("{}/{}", isp.published[0], isp.published[1]),
        format_args!("{}/{}", isp.dropped[0], isp.dropped[1]),
        format_args!("{}/{}", isp.readers[0], isp.readers[1]),
        isp_socket(0),
        isp_socket(1),
        // The service runs no 3A: a processed stream has the configured exposure and gain and
        // the ISP's neutral settings.
        if isp.asked == crate::config::IspMode::None {
            "-"
        } else {
            "none"
        },
        if isp.reason.is_some() {
            isp.reason_at_ns.to_string()
        } else {
            "-".to_string()
        },
        isp_reason,
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
    /// The configured keep window (`keep=`), the most the window grows to.
    pub keep: Option<u64>,
    /// The keep window in force now (`kept=`).
    pub kept: Option<u64>,
    /// Readers cut off for the reserve (`reserve_cuts=`).
    pub reserve_cuts: Option<u64>,
    /// Publishes on which the reserve could not be kept (`bound_unmet=`) even after every holder of
    /// the frame was cut off, adopters included. Zero on a working ring.
    pub bound_unmet: Option<u64>,
    /// Frame-stream discontinuities: each lost resume (the camera was bound again and the sequence
    /// restarts). Not gaps: `skipped` does not count them.
    pub discontinuities: Option<u64>,
    /// The device contract another build's that refuses the camera, by name (`contract_mismatch=`).
    pub contract_mismatch: Option<String>,
    /// The camera's controls count (`controls_seq=`): read `controls` again when it moves.
    pub controls_seq: Option<u64>,
    /// The ISP that made the last processed frame (`isp=`: `pisp`, `soft`, `none`).
    pub isp: Option<String>,
    /// The config's `isp` (`isp_asked=`).
    pub isp_asked: Option<String>,
    /// How raw frames go into the back end's input (`isp_form=`).
    pub isp_form: Option<String>,
    /// Back-end jobs refused (`isp_refused=`).
    pub isp_refused: Option<u64>,
    /// Raw frames the processed stream has no picture for (`isp_skipped=`).
    pub isp_skipped: Option<u64>,
    /// Switches to the soft ISP and back (`isp_switches=<to_soft>/<to_pisp>`).
    pub isp_switches: Option<(u64, u64)>,
    /// Frames published on isp0 and isp1 (`isp_published=a/b`).
    pub isp_published: Option<(u64, u64)>,
    /// Readers on isp0 and isp1 (`isp_readers=a/b`).
    pub isp_readers: Option<(u64, u64)>,
    /// The processed sockets (`isp0_socket=`, `isp1_socket=`).
    pub isp_sockets: [Option<PathBuf>; 2],
    /// The 3A a processed stream has (`isp_3a=`): `none` in this service.
    pub isp_3a: Option<String>,
    /// When the ISP reason was noted, CLOCK_MONOTONIC ns (`isp_reason_at=`).
    pub isp_reason_at_ns: Option<u64>,
    /// Why the ISP in force is not the one asked for, or the last refusal (`isp_reason=`, one word:
    /// spaces are `_`).
    pub isp_reason: Option<String>,
    /// The last failure's message: the free text at the end of the line.
    pub last_error: Option<String>,
}

/// One `copy copies=.. bytes=.. site=..` line of a `list` answer: a site where frame bytes were
/// copied, in the service's process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyLine {
    pub copies: u64,
    /// Bytes written by the copies.
    pub bytes: u64,
    /// Bytes read, for a site whose copy changes the form (`bytes_read=`; `None` when the line
    /// does not say: the same as `bytes`).
    pub bytes_read: Option<u64>,
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
            "keep" => camera.keep = number,
            "kept" => camera.kept = number,
            "reserve_cuts" => camera.reserve_cuts = number,
            "bound_unmet" => camera.bound_unmet = number,
            "discontinuities" => camera.discontinuities = number,
            "contract_mismatch" => camera.contract_mismatch = text,
            "controls_seq" => camera.controls_seq = number,
            "isp" => camera.isp = text,
            "isp_asked" => camera.isp_asked = text,
            "isp_form" => camera.isp_form = text,
            "isp_refused" => camera.isp_refused = number,
            "isp_skipped" => camera.isp_skipped = number,
            "isp_switches" => camera.isp_switches = pair(value),
            "isp_published" => camera.isp_published = pair(value),
            "isp_readers" => camera.isp_readers = pair(value),
            "isp0_socket" => camera.isp_sockets[0] = text.map(PathBuf::from),
            "isp1_socket" => camera.isp_sockets[1] = text.map(PathBuf::from),
            "isp_3a" => camera.isp_3a = text,
            "isp_reason_at" => camera.isp_reason_at_ns = number,
            "isp_reason" => camera.isp_reason = text,
            _ => {}
        }
    }
    camera.last_error = last_error.filter(|text| *text != "-").map(str::to_string);
    Some(camera)
}

/// `<a>/<b>` as two numbers.
fn pair(value: &str) -> Option<(u64, u64)> {
    let (a, b) = value.split_once('/')?;
    Some((a.parse().ok()?, b.parse().ok()?))
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
    let (mut copies, mut bytes, mut bytes_read) = (None, None, None);
    for word in head.split_ascii_whitespace() {
        match word.split_once('=') {
            Some(("copies", v)) => copies = v.parse().ok(),
            Some(("bytes", v)) => bytes = v.parse().ok(),
            Some(("bytes_read", v)) => bytes_read = v.parse().ok(),
            _ => {}
        }
    }
    Some(CopyLine {
        copies: copies?,
        bytes: bytes?,
        bytes_read,
        site: site.to_string(),
    })
}

/// One `control` line of a `controls` answer: a standard control as this camera has it. A field
/// shown as `unknown` (the backend cannot say) is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlLine {
    /// The control's name, as `Control::name` gives it.
    pub name: String,
    pub value: Shown,
    /// The control's documented unit (`-` for none).
    pub unit: Option<String>,
    pub range: Option<Span>,
    pub step: Option<u32>,
    pub default: Option<Shown>,
    pub effective: Option<(u32, u32)>,
    pub auto: Option<bool>,
    pub access: Access,
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

/// One `device` line of a `controls` (or `getd`) answer with its `entry` lines: a control the camera
/// has, in the device's own units, by the backend's id. A field shown as `unknown` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceLine {
    pub id: u32,
    pub name: String,
    pub kind: DeviceKind,
    pub access: Access,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub step: Option<u64>,
    pub default: Option<i64>,
    pub effective: Option<(i64, i64)>,
    pub value: Option<i64>,
    /// The standard control's name, when the device control is one.
    pub standard: Option<String>,
    pub menu: Vec<MenuEntry>,
    /// A `kind=float` line's numbers (its integer fields are `None`).
    pub real: Option<RealValues>,
    /// A `kind=rect` line's rectangles (its integer fields are `None`).
    pub rect: Option<RectValues>,
}

/// A whole `controls` answer: its count, the standard controls and the device controls.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ControlsAnswer {
    /// The `seq` line: the camera's `controls_seq` when the answer was made.
    pub seq: Option<u64>,
    pub standard: Vec<ControlLine>,
    pub device: Vec<DeviceLine>,
}

/// Reads a `controls` answer (its lines, with or without the closing `ok`): the `control` lines.
pub fn parse_controls<S: AsRef<str>>(lines: &[S]) -> Vec<ControlLine> {
    parse_controls_answer(lines).standard
}

/// Reads a whole `controls` or `getd` answer. Unknown line kinds and fields are skipped; an `entry`
/// line belongs to the `device` line before it with the same id.
pub fn parse_controls_answer<S: AsRef<str>>(lines: &[S]) -> ControlsAnswer {
    let mut answer = ControlsAnswer::default();
    for line in lines {
        let line = line.as_ref();
        let mut words = line.split_ascii_whitespace();
        match words.next() {
            Some("seq") => answer.seq = words.next().and_then(|n| n.parse().ok()),
            Some("control") => answer.standard.extend(parse_control_line(line)),
            Some("device") => answer.device.extend(parse_device_line(line)),
            Some("entry") => {
                let (Some(id), Some(value)) = (
                    words.next().and_then(parse_device_id),
                    words.next().and_then(|v| v.parse::<i64>().ok()),
                ) else {
                    continue;
                };
                let label = words.collect::<Vec<_>>().join(" ");
                if let Some(device) = answer.device.last_mut().filter(|d| d.id == id) {
                    device.menu.push(MenuEntry {
                        value,
                        label: (!label.is_empty()).then_some(label),
                    });
                }
            }
            _ => {}
        }
    }
    answer
}

fn known<T: std::str::FromStr>(text: &str) -> Option<T> {
    (text != "unknown").then(|| text.parse().ok()).flatten()
}

fn parse_control_line(line: &str) -> Option<ControlLine> {
    let mut words = line.split_ascii_whitespace();
    if words.next()? != "control" {
        return None;
    }
    let mut parsed = ControlLine {
        name: words.next()?.to_string(),
        value: Shown::NoFrame,
        unit: None,
        range: None,
        step: None,
        default: None,
        effective: None,
        auto: None,
        access: Access::ReadWrite,
    };
    let mut value = None;
    for word in words {
        let Some((key, text)) = word.split_once('=') else {
            continue;
        };
        match key {
            "value" => value = parse_shown(text),
            "unit" => parsed.unit = (text != "-").then(|| text.to_string()),
            "range" => parsed.range = parse_span(text),
            "step" => parsed.step = known(text),
            "default" => parsed.default = (text != "unknown").then(|| parse_shown(text)).flatten(),
            "effective" => parsed.effective = parse_pair(text),
            "auto" => parsed.auto = known(text),
            "access" => parsed.access = Access::from_name(text).unwrap_or_default(),
            _ => {}
        }
    }
    parsed.value = value?;
    Some(parsed)
}

fn parse_device_line(line: &str) -> Option<DeviceLine> {
    let (head, name) = match line.find(" name=") {
        Some(at) => (&line[..at], line[at + " name=".len()..].to_string()),
        None => (line, String::new()),
    };
    let mut words = head.split_ascii_whitespace();
    if words.next()? != "device" {
        return None;
    }
    let mut parsed = DeviceLine {
        id: parse_device_id(words.next()?)?,
        name,
        kind: DeviceKind::Other,
        access: Access::ReadWrite,
        min: None,
        max: None,
        step: None,
        default: None,
        effective: None,
        value: None,
        standard: None,
        menu: Vec::new(),
        real: None,
        rect: None,
    };
    // The numbers are read once the kind is known: an integer, a real or a rectangle.
    let [mut min, mut max, mut default, mut effective, mut value] = [""; 5];
    for word in words {
        let Some((key, text)) = word.split_once('=') else {
            continue;
        };
        match key {
            "kind" => parsed.kind = kind_named(text).unwrap_or(DeviceKind::Other),
            "access" => parsed.access = Access::from_name(text).unwrap_or_default(),
            "min" => min = text,
            "max" => max = text,
            "step" => parsed.step = known(text),
            "default" => default = text,
            "effective" => effective = text,
            "value" => value = text,
            "standard" => parsed.standard = (text != "-").then(|| text.to_string()),
            _ => {}
        }
    }
    match parsed.kind {
        DeviceKind::Float => {
            let real = |t: &str| known::<f32>(t).filter(|v| v.is_finite()).map(Real);
            parsed.real = Some(RealValues {
                min: real(min),
                max: real(max),
                default: real(default),
                effective: effective
                    .split_once("..")
                    .and_then(|(lo, hi)| real(lo).zip(real(hi))),
                value: real(value),
            });
        }
        DeviceKind::Rect => {
            parsed.rect = Some(RectValues {
                min: parse_rect(min),
                max: parse_rect(max),
                default: parse_rect(default),
                value: parse_rect(value),
            });
        }
        _ => {
            parsed.min = known(min);
            parsed.max = known(max);
            parsed.default = known(default);
            parsed.effective = parse_pair(effective);
            parsed.value = known(value);
        }
    }
    Some(parsed)
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

fn span(span: Span) -> String {
    match span {
        Span::Manual { min, max } => format!("{min}..{max}"),
        Span::Flag => "flag".to_string(),
        Span::Rect => "rect".to_string(),
    }
}

fn parse_span(text: &str) -> Option<Span> {
    match text {
        "flag" => Some(Span::Flag),
        "rect" => Some(Span::Rect),
        _ => {
            let (min, max) = text.split_once("..")?;
            Some(Span::Manual {
                min: min.parse().ok()?,
                max: max.parse().ok()?,
            })
        }
    }
}

fn parse_pair<T: std::str::FromStr>(text: &str) -> Option<(T, T)> {
    let (lo, hi) = text.split_once("..")?;
    Some((lo.parse().ok()?, hi.parse().ok()?))
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
    fn a_contract_mismatch_is_its_name_and_its_sentence_is_the_text_with_the_fault_code() {
        // The camera the board's Lemnos refuses: the contract's name as a field, the fault code, and
        // the sentence the Lemnos error gives (both hashes) as the text.
        let slot = slot("lemnos:cam0");
        let sentence = "this Styx and the board's Lemnos are different builds: contract camera.sensor/1: a base method is laid out differently (the device's base hash 0x4d6f4b4bddf78636, this client's 0x33b860b02987233d)";
        slot.set_contract(Some("camera.sensor/1".to_string()));
        slot.failed(sentence.to_string(), 1);
        let line = camera_line(&slot.snapshot());
        assert!(
            line.contains(
                "fault=12 bound_unmet=0 discontinuities=0 contract_mismatch=camera.sensor/1 controls_seq=0 isp=none isp_asked=none isp_form=- isp_refused=0 isp_skipped=0 isp_switches=0/0 isp_published=0/0 isp_dropped=0/0 isp_readers=0/0 isp0_socket=- isp1_socket=- isp_3a=- isp_reason_at=- isp_reason=- last_error=this Styx and the board's Lemnos are different builds: contract camera.sensor/1"
            ),
            "{line}"
        );
        let cam = parse_camera_line(&line).expect("a camera line");
        assert_eq!(cam.contract_mismatch.as_deref(), Some("camera.sensor/1"));
        assert_eq!(cam.fault, Some(12));
        assert_eq!(cam.last_error.as_deref(), Some(sentence));
        slot.set_contract(None);
        let cam = parse_camera_line(&camera_line(&slot.snapshot())).expect("a camera line");
        assert_eq!(cam.contract_mismatch, None);
        assert_eq!(cam.fault, None);
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
                "layout_refused=marks:1:2 fault=1 bound_unmet=0 discontinuities=0 contract_mismatch=- controls_seq=0 isp=none isp_asked=none isp_form=- isp_refused=0 isp_skipped=0 isp_switches=0/0 isp_published=0/0 isp_dropped=0/0 isp_readers=0/0 isp0_socket=- isp1_socket=- isp_3a=- isp_reason_at=- isp_reason=- last_error=frame stream cam0: the marks page"
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
            "copy copies=2 bytes=4096 bytes_read=2560 site=pisp-be-input",
            "ok",
        ];
        let listing = parse_list(&lines);
        assert_eq!(listing.cameras.len(), 1);
        assert_eq!(listing.cameras[0].owner.as_deref(), Some("native"));
        assert_eq!(
            listing.copies,
            vec![
                CopyLine {
                    copies: 4,
                    bytes: 64,
                    bytes_read: None,
                    site: "a site with spaces".to_string()
                },
                CopyLine {
                    copies: 2,
                    bytes: 4096,
                    bytes_read: Some(2560),
                    site: "pisp-be-input".to_string()
                }
            ]
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
    fn a_cameras_controls_read_back_field_for_field_with_menus_and_unknowns() {
        use styx_select::ControlsInfo;
        let slot = slot("cam");
        let info = ControlsInfo {
            standard: vec![
                StandardInfo {
                    control: Control::Exposure,
                    range: Some(Span::Manual {
                        min: 9,
                        max: 472_973,
                    }),
                    step: None,
                    default: Some(ControlValue::Manual(10_000)),
                    effective: Some((9, 151_437)),
                    auto: Some(false),
                    access: Access::ReadWrite,
                },
                StandardInfo {
                    access: Access::Inactive,
                    ..StandardInfo::unknown(Control::HFlip)
                },
            ],
            device: vec![
                DeviceInfo {
                    id: 0x0098_0918,
                    name: "Power Line Frequency".to_string(),
                    kind: DeviceKind::Menu,
                    min: Some(0),
                    max: Some(3),
                    step: Some(1),
                    default: Some(1),
                    effective: Some((0, 3)),
                    value: Some(2),
                    standard: Some(Control::PowerLineFrequency),
                    access: Access::ReadWrite,
                    menu: vec![
                        MenuEntry {
                            value: 0,
                            label: Some("Disabled".to_string()),
                        },
                        MenuEntry {
                            value: 2,
                            label: Some("60 Hz".to_string()),
                        },
                        MenuEntry {
                            value: 3,
                            label: None,
                        },
                    ],
                    real: None,
                    rect: None,
                },
                DeviceInfo {
                    id: 1,
                    name: "Exposure, Dynamic Framerate".to_string(),
                    kind: DeviceKind::Boolean,
                    min: None,
                    max: None,
                    step: None,
                    default: None,
                    effective: None,
                    value: None,
                    standard: None,
                    access: Access::ReadOnly,
                    menu: Vec::new(),
                    real: None,
                    rect: None,
                },
                DeviceInfo {
                    id: 0x0000_0003,
                    name: "Brightness".to_string(),
                    kind: DeviceKind::Float,
                    min: None,
                    max: None,
                    step: None,
                    default: None,
                    effective: None,
                    value: None,
                    standard: Some(Control::Brightness),
                    access: Access::ReadWrite,
                    menu: Vec::new(),
                    real: Some(RealValues {
                        min: Some(Real(-1.0)),
                        max: Some(Real(1.0)),
                        default: Some(Real(0.0)),
                        effective: Some((Real(-1.0), Real(1.0))),
                        value: Some(Real(1e-7)),
                    }),
                    rect: None,
                },
                DeviceInfo {
                    id: 0x0000_0004,
                    name: "ScalerCrop".to_string(),
                    kind: DeviceKind::Rect,
                    min: None,
                    max: None,
                    step: None,
                    default: None,
                    effective: None,
                    value: None,
                    standard: Some(Control::Crop),
                    access: Access::ReadWrite,
                    menu: Vec::new(),
                    real: None,
                    rect: Some(RectValues {
                        min: Some(Rect {
                            x: 0,
                            y: 0,
                            width: 64,
                            height: 48,
                        }),
                        max: Some(Rect {
                            x: 0,
                            y: 0,
                            width: 4056,
                            height: 3040,
                        }),
                        default: None,
                        value: None,
                    }),
                },
            ],
        };
        slot.set_controls_info(info.clone());
        let lines = controls(&slot);
        assert_eq!(lines[0], "seq 1");
        let answer = parse_controls_answer(&lines);
        assert_eq!(answer.seq, Some(1));
        assert_eq!(answer.standard.len(), 2);
        for (line, info) in answer.standard.iter().zip(&info.standard) {
            assert_eq!(line.name, info.control.name());
            assert_eq!(line.range, info.range);
            assert_eq!(line.step, info.step);
            assert_eq!(line.default, info.default.map(Shown::Value));
            assert_eq!(line.effective, info.effective);
            assert_eq!(line.auto, info.auto);
            assert_eq!(line.access, info.access);
            assert_eq!(line.value, Shown::NoFrame);
        }
        assert_eq!(answer.standard[1].unit, None, "a flag has no unit");
        assert_eq!(answer.device.len(), 4);
        for (line, info) in answer.device.iter().zip(&info.device) {
            assert_eq!(line.id, info.id);
            assert_eq!(line.name, info.name, "free text keeps its spaces");
            assert_eq!(line.kind, info.kind);
            assert_eq!(line.access, info.access);
            assert_eq!(
                (
                    line.min,
                    line.max,
                    line.step,
                    line.default,
                    line.effective,
                    line.value
                ),
                (
                    info.min,
                    info.max,
                    info.step,
                    info.default,
                    info.effective,
                    info.value
                )
            );
            assert_eq!(line.standard.as_deref(), info.standard.map(Control::name));
            assert_eq!(line.menu, info.menu);
            assert_eq!((line.real, line.rect), (info.real, info.rect));
        }
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("device 0x00000003 kind=float")
                    && l.contains(" min=-1.0 max=1.0 ")
                    && l.contains(" default=0.0 effective=-1.0..1.0 value=1e-7 ")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("kind=rect") && l.contains(" min=0,0,64x48 max=0,0,4056x3040 ")),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"entry 0x00980918 3".to_string()),
            "{lines:?}"
        );
        // The count shows on the camera's list line too, and moves once per change.
        slot.controls_changed();
        let camera = parse_camera_line(&camera_line(&slot.snapshot())).expect("a camera line");
        assert_eq!(camera.controls_seq, Some(2));
    }

    #[test]
    fn device_ids_read_in_decimal_or_hex() {
        assert_eq!(parse_device_id("0x00980918"), Some(0x0098_0918));
        assert_eq!(parse_device_id("1"), Some(1));
        assert_eq!(parse_device_id("0xzz"), None);
        assert_eq!(parse_device_id("-1"), None);
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

    /// `setd` on a float takes a decimal with a `.` or an exponent, inside the camera's bounds; an
    /// integer, NaN and the infinities are the wrong kind. An integer control keeps its integers.
    #[test]
    fn a_float_device_control_is_set_by_a_real_inside_its_bounds() {
        let float = DeviceInfo {
            id: 3,
            name: "Brightness".to_string(),
            kind: DeviceKind::Float,
            min: None,
            max: None,
            step: None,
            default: None,
            effective: None,
            value: None,
            standard: None,
            access: Access::ReadWrite,
            menu: Vec::new(),
            real: Some(RealValues {
                min: Some(Real(-1.0)),
                max: Some(Real(1.0)),
                ..RealValues::default()
            }),
            rect: None,
        };
        assert_eq!(device_set(&float, "0.5"), Ok(DeviceSet::Real(0.5)));
        assert_eq!(device_set(&float, "-1e-3"), Ok(DeviceSet::Real(-1e-3)));
        assert_eq!(device_set(&float, "1.0"), Ok(DeviceSet::Real(1.0)));
        let kind = Refusal::Kind.to_string();
        for wrong in ["1", "0", "NaN", "inf", "-inf", "1e40", "x"] {
            assert_eq!(device_set(&float, wrong), Err(kind.clone()), "{wrong}");
        }
        assert_eq!(
            device_set(&float, "1.5"),
            Err("OutOfRange { min: -1.0, max: 1.0, value: 1.5 }".to_string())
        );
        // A bound the camera does not give is not checked.
        let open = DeviceInfo {
            real: Some(RealValues::default()),
            ..float.clone()
        };
        assert_eq!(device_set(&open, "1000.0"), Ok(DeviceSet::Real(1000.0)));
        let read_only = DeviceInfo {
            access: Access::ReadOnly,
            ..float.clone()
        };
        assert_eq!(
            device_set(&read_only, "0.5"),
            Err("the control is read_only".to_string())
        );
        let integer = DeviceInfo {
            kind: DeviceKind::Integer,
            min: Some(0),
            max: Some(10),
            real: None,
            ..float
        };
        assert_eq!(device_set(&integer, "4"), Ok(DeviceSet::Integer(4)));
        assert!(device_set(&integer, "0.5").is_err());
        let rect = DeviceInfo {
            kind: DeviceKind::Rect,
            real: None,
            ..integer
        };
        assert_eq!(device_set(&rect, "1"), Err(kind));
    }
}
