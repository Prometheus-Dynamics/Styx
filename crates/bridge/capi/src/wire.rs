//! The camera service's control socket, as a client: one line per request, answer lines ending in
//! `ok` or `err <why>`. The protocol is `styx-service`'s (its `control` module documents it); this
//! file speaks it and keeps no state of the service's.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use styx_controls::{Control, ControlValue};
use styx_service::control::Shown;

use crate::status::{
    E_INVALID, E_IO, E_NOT_FOUND, E_PROTOCOL, E_STOPPED, E_UNSUPPORTED, Fail, Res,
};

/// How long one request may take on the socket before the call fails.
pub(crate) const IO_WAIT: Duration = Duration::from_secs(5);
/// The most answer lines one request may have (a controls answer has one line per control).
const MAX_ANSWER: usize = 256;

/// One camera as a `list` answer names it: the id, its owner and its frame socket.
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

/// The cameras of a `list` answer, read by the service's own reader (unknown lines and fields are
/// skipped there).
pub(crate) fn entries(lines: &[String]) -> Vec<Entry> {
    styx_service::control::parse_list(lines)
        .cameras
        .into_iter()
        .map(|cam| Entry {
            name: cam.id,
            owner: cam.owner.unwrap_or_else(|| "-".to_string()),
            socket: cam.socket,
        })
        .collect()
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

/// The value in a `controls` answer for `control`, as the service's reader gives it.
pub(crate) fn value_of(lines: &[String], control: Control) -> Res<ControlValue> {
    let line = styx_service::control::parse_controls(lines)
        .into_iter()
        .find(|line| line.name == control.name())
        .ok_or_else(|| Fail::new(E_NOT_FOUND, "the service lists no such control"))?;
    match line.value {
        Shown::Value(value) => Ok(value),
        Shown::NoFrame => Err(Fail::new(E_NOT_FOUND, "no frame yet, so no value")),
        Shown::Rect => Err(Fail::new(
            E_UNSUPPORTED,
            "a crop is not read through the C surface",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cameras_come_from_the_services_reader_with_their_socket_and_owner() {
        let lines = vec![
            "camera lemnos:cam0 state=streaming owner=native frames=5 socket=/tmp/s/a-1.sock \
             publish=publishing health=healthy last_error=-"
                .to_string(),
            "camera x state=absent owner=- socket=-".to_string(),
            "copy copies=0 bytes=0 site=styx_frame".to_string(),
            "future kind of line".to_string(),
        ];
        let found = entries(&lines);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "lemnos:cam0");
        assert_eq!(found[0].owner, "native");
        assert_eq!(found[0].socket, Some(PathBuf::from("/tmp/s/a-1.sock")));
        assert_eq!(found[1].socket, None);
    }

    #[test]
    fn a_value_is_read_from_its_control_line_and_refusals_are_mapped() {
        let lines = vec![
            "control exposure unit=us range=1..9 default=10000 auto=true value=5000".to_string(),
            "control gain unit=- range=1..9 default=1 auto=false value=-".to_string(),
        ];
        assert_eq!(
            value_of(&lines, Control::Exposure),
            Ok(ControlValue::Manual(5000))
        );
        assert_eq!(
            value_of(&lines, Control::Gain).map_err(|f| f.status),
            Err(E_NOT_FOUND)
        );
        assert!(value_of(&lines, Control::FrameRate).is_err());
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
