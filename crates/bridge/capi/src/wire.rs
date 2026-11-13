//! The camera service's control socket, as a client: one line per request, answer lines ending in
//! `ok` or `err <why>`. The protocol is `styx-service`'s (its `control` module documents it); this
//! file speaks it and keeps no state of the service's.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use styx_controls::{Control, ControlValue};

use crate::status::{
    E_INVALID, E_IO, E_NOT_FOUND, E_PROTOCOL, E_STOPPED, E_UNSUPPORTED, Fail, Res,
};

/// How long one request may take on the socket before the call fails.
pub(crate) const IO_WAIT: Duration = Duration::from_secs(5);
/// The most answer lines one request may have (a controls answer has one line per control).
const MAX_ANSWER: usize = 256;

/// One camera as the service's `list` line names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) owner: String,
    /// The camera's frame socket, when it is publishing.
    pub(crate) socket: Option<PathBuf>,
}

/// A connection to the control socket.
#[derive(Debug)]
pub(crate) struct ControlLink {
    lines: BufReader<UnixStream>,
}

impl ControlLink {
    pub(crate) fn connect(path: &Path) -> Res<Self> {
        let stream = UnixStream::connect(path)
            .map_err(|e| Fail::io(format!("control socket {}: {e}", path.display())))?;
        stream.set_read_timeout(Some(IO_WAIT)).map_err(Fail::io)?;
        stream.set_write_timeout(Some(IO_WAIT)).map_err(Fail::io)?;
        Ok(Self {
            lines: BufReader::new(stream),
        })
    }

    /// Sends one request and returns its answer lines (without the closing `ok`).
    pub(crate) fn ask(&mut self, request: &str) -> Res<Vec<String>> {
        writeln!(self.lines.get_mut(), "{request}").map_err(Fail::io)?;
        let mut answer = Vec::new();
        loop {
            let mut line = String::new();
            let read = self.lines.read_line(&mut line).map_err(Fail::io)?;
            if read == 0 {
                return Err(Fail::new(E_STOPPED, "the control socket closed"));
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line == "ok" {
                return Ok(answer);
            }
            if let Some(why) = line.strip_prefix("err") {
                return Err(service_error(why.trim()));
            }
            if answer.len() >= MAX_ANSWER {
                return Err(Fail::new(
                    E_PROTOCOL,
                    "the control socket sent too many lines",
                ));
            }
            answer.push(line.to_string());
        }
    }
}

/// The service's refusal, as a status.
fn service_error(why: &str) -> Fail {
    let status = if why.starts_with("no camera") {
        E_NOT_FOUND
    } else if why.contains("not streaming") || why.contains("absent") || why.contains("ended") {
        E_STOPPED
    } else if why.contains("unsupported") {
        E_UNSUPPORTED
    } else if why.starts_with("no control") {
        E_INVALID
    } else {
        E_IO
    };
    Fail::new(status, format!("control socket: {why}"))
}

/// The cameras of a `list` answer.
pub(crate) fn parse_list(lines: &[String]) -> Res<Vec<Entry>> {
    lines.iter().map(|line| parse_camera(line)).collect()
}

/// One `camera <id> key=value ...` line. The id is the second word; the free text `last_error`
/// comes last and is not read.
pub(crate) fn parse_camera(line: &str) -> Res<Entry> {
    let mut words = line.split_ascii_whitespace();
    if words.next() != Some("camera") {
        return Err(Fail::new(
            E_PROTOCOL,
            format!("not a camera line: {line:.80}"),
        ));
    }
    let name = words
        .next()
        .ok_or_else(|| Fail::new(E_PROTOCOL, "a camera line with no id"))?;
    let mut owner = String::from("-");
    let mut socket = None;
    for word in words {
        if word.starts_with("last_error=") {
            break;
        }
        if let Some(value) = word.strip_prefix("owner=") {
            owner = value.to_string();
        } else if let Some(value) = word.strip_prefix("socket=")
            && value != "-"
        {
            socket = Some(PathBuf::from(value));
        }
    }
    Ok(Entry {
        name: name.to_string(),
        owner,
        socket,
    })
}

/// The wire text of a control value, as `set` takes it.
pub(crate) fn value_text(value: ControlValue) -> Res<String> {
    match value {
        ControlValue::Auto => Ok("auto".to_string()),
        ControlValue::Manual(v) => Ok(v.to_string()),
        ControlValue::Flag(true) => Ok("on".to_string()),
        ControlValue::Flag(false) => Ok("off".to_string()),
        ControlValue::Crop(_) => Err(Fail::new(
            E_UNSUPPORTED,
            "a crop is not settable through the C surface",
        )),
    }
}

/// The value in a `controls` answer's `value=` for `control`.
pub(crate) fn value_of(lines: &[String], control: Control) -> Res<ControlValue> {
    let prefix = format!("control {} ", control.name());
    let line = lines
        .iter()
        .find(|l| l.starts_with(&prefix))
        .ok_or_else(|| Fail::new(E_NOT_FOUND, "the service lists no such control"))?;
    let text = line
        .split_ascii_whitespace()
        .find_map(|word| word.strip_prefix("value="))
        .ok_or_else(|| Fail::new(E_PROTOCOL, "a control line with no value"))?;
    match text {
        "auto" => Ok(ControlValue::Auto),
        "on" => Ok(ControlValue::Flag(true)),
        "off" => Ok(ControlValue::Flag(false)),
        "-" => Err(Fail::new(E_NOT_FOUND, "no frame yet, so no value")),
        "rect" => Err(Fail::new(
            E_UNSUPPORTED,
            "a crop is not read through the C surface",
        )),
        number => number
            .parse::<u32>()
            .map(ControlValue::Manual)
            .map_err(|_| Fail::new(E_PROTOCOL, "a control value that is not a number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_camera_line_gives_its_id_owner_and_socket_and_stops_at_last_error() {
        let line = "camera lemnos:cam0-sensor state=streaming owner=native starts=1 failures=0 \
                    total_failures=0 frames=5 skipped=0 last_id=5 next_delay_ms=- \
                    socket=/tmp/s/cameras/a-1.sock publish=publishing readers=0 published=5 \
                    publish_dropped=0 health=healthy last_error=- socket=/wrong";
        let entry = parse_camera(line).expect("parse");
        assert_eq!(entry.name, "lemnos:cam0-sensor");
        assert_eq!(entry.owner, "native");
        assert_eq!(entry.socket, Some(PathBuf::from("/tmp/s/cameras/a-1.sock")));
        let unpublished = parse_camera("camera x state=absent owner=- socket=-").expect("parse");
        assert_eq!(unpublished.socket, None);
    }

    #[test]
    fn a_value_is_read_from_its_control_line_and_refusals_are_mapped() {
        let lines = vec![
            "control exposure unit=us range=1..9 default=10000 auto=true value=5000".to_string(),
        ];
        assert_eq!(
            value_of(&lines, Control::Exposure),
            Ok(ControlValue::Manual(5000))
        );
        assert!(value_of(&lines, Control::Gain).is_err());
        assert_eq!(service_error("no camera x").status, E_NOT_FOUND);
        assert_eq!(
            service_error("camera is absent, not streaming").status,
            E_STOPPED
        );
        assert_eq!(
            service_error("unsupported by this backend: x").status,
            E_UNSUPPORTED
        );
    }
}
