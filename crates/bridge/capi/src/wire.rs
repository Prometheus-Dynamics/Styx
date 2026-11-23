//! The camera service's control socket, through `styx-control-client`: one connection per request
//! (the service serves one connection at a time and hangs up on an idle one), its errors as the C
//! surface's statuses.

use std::path::{Path, PathBuf};
use std::time::Duration;

use styx_control_client::{ControlClient, Error, ErrorKind, Options};
use styx_controls::{Control, ControlValue};
use styx_select::{ControlsInfo, Refusal};
use styx_service::control::parse_controls_answer;

use crate::status::{
    E_ACCESS, E_INVALID, E_IO, E_NOT_FOUND, E_PROTOCOL, E_RANGE, E_STOPPED, E_UNSUPPORTED, Fail,
    Res,
};

/// How long one request may take on a socket before the call fails.
pub(crate) const IO_WAIT: Duration = Duration::from_secs(5);

/// One camera as a `list` answer names it: the id, its owner and its frame socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) owner: String,
    /// The camera's frame socket, when it is publishing.
    pub(crate) socket: Option<PathBuf>,
}

/// The control socket. It holds no connection between requests.
#[derive(Debug)]
pub(crate) struct ControlLink {
    client: ControlClient,
}

impl ControlLink {
    pub(crate) fn connect(path: &Path) -> Res<Self> {
        Ok(Self {
            client: ControlClient::with_options(
                path,
                Options {
                    timeout: IO_WAIT,
                    ..Options::default()
                },
            ),
        })
    }

    /// Sends one request and returns its answer lines (without the closing `ok`).
    pub(crate) fn ask(&mut self, request: &str) -> Res<Vec<String>> {
        self.client.ask(request).map_err(fail)
    }
}

/// A client error as a status.
fn fail(error: Error) -> Fail {
    let status = match error.kind {
        ErrorKind::NotFound => E_NOT_FOUND,
        ErrorKind::Range => E_RANGE,
        ErrorKind::Access => E_ACCESS,
        ErrorKind::Invalid => E_INVALID,
        ErrorKind::Stopped => E_STOPPED,
        ErrorKind::Unsupported => E_UNSUPPORTED,
        ErrorKind::Protocol => E_PROTOCOL,
        ErrorKind::Unreachable | ErrorKind::Timeout | ErrorKind::Io | ErrorKind::Service => E_IO,
    };
    Fail::new(status, error.message)
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
    styx_control_client::value_text(value).map_err(fail)
}

/// A camera's refusal of a value, as a status: the same statuses the service's refusal text maps
/// to.
pub(crate) fn refusal(refusal: Refusal) -> Fail {
    fail(Error::refusal(refusal))
}

/// A `controls` answer as the camera's controls: its `seq` (0 when the service gives none) and the
/// lines read back into the types the in-process path has.
pub(crate) fn controls_of(lines: &[String]) -> (u64, ControlsInfo) {
    styx_control_client::info_of(&parse_controls_answer(lines))
}

/// The value in a `controls` answer for `control`.
pub(crate) fn value_of(lines: &[String], control: Control) -> Res<ControlValue> {
    styx_control_client::value_of(&parse_controls_answer(lines), control).map_err(fail)
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

    /// The service's refusals come back as the same statuses the in-process check gives.
    #[test]
    fn refusals_and_service_errors_have_the_c_statuses() {
        use styx_select::Access;
        for (text, refused, status) in [
            (
                "OutOfRange { min: 1000, max: 15938, value: 16000 }",
                Refusal::OutOfRange {
                    min: 1_000,
                    max: 15_938,
                    value: 16_000,
                },
                E_RANGE,
            ),
            (
                "the control is read_only",
                Refusal::Access(Access::ReadOnly),
                E_ACCESS,
            ),
            (
                "the value is the wrong kind for this control",
                Refusal::Kind,
                E_INVALID,
            ),
        ] {
            assert_eq!(fail(Error::from_service(text)).status, status, "{text}");
            assert_eq!(refusal(refused).status, status, "{text}");
        }
        for (text, status) in [
            ("no camera x", E_NOT_FOUND),
            ("the camera has no device control 0xdead", E_NOT_FOUND),
            ("camera is absent, not streaming", E_STOPPED),
            ("unsupported by this backend: x", E_UNSUPPORTED),
            ("something new", E_IO),
        ] {
            assert_eq!(fail(Error::from_service(text)).status, status, "{text}");
        }
        let lines = vec![
            "control exposure unit=us range=1..9 default=10000 auto=true value=5000".to_string(),
        ];
        assert_eq!(
            value_of(&lines, Control::Exposure),
            Ok(ControlValue::Manual(5000))
        );
        assert_eq!(
            value_of(&lines, Control::Gain).map_err(|f| f.status),
            Err(E_NOT_FOUND)
        );
    }
}
