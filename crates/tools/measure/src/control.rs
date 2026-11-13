//! The client of the service's control socket (`styx-service`'s line protocol): one request, its
//! answer lines up to the `ok` or `err` line. Read-only here: `list` only.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// How long a request may take before the client gives up.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Sends `request` and returns the answer lines, the last one `ok` or `err ...`.
pub fn ask(socket: &Path, request: &str) -> io::Result<Vec<String>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    writeln!(stream, "{request}")?;
    let mut reader = BufReader::new(stream);
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the control socket closed before its answer ended",
            ));
        }
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        let done = line == "ok" || line.starts_with("err ") || line == "err";
        lines.push(line);
        if done {
            return Ok(lines);
        }
    }
}

/// One `camera <id> key=value ...` line of the `list` answer, parsed for the fields the report
/// uses. `last_error` and any free text come last; the parse stops at its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraLine {
    pub raw: String,
    pub id: String,
    pairs: Vec<(String, String)>,
}

impl CameraLine {
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_ascii_whitespace();
        if words.next()? != "camera" {
            return None;
        }
        let id = words.next()?.to_string();
        let mut pairs = Vec::new();
        for word in words {
            let Some((key, value)) = word.split_once('=') else {
                continue;
            };
            pairs.push((key.to_string(), value.to_string()));
        }
        Some(Self {
            raw: line.to_string(),
            id,
            pairs,
        })
    }

    /// The value of `key`, when the line has it (`-` is returned as it is written).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The value of `key` as a number, when it is one.
    pub fn number(&self, key: &str) -> Option<u64> {
        self.get(key)?.parse().ok()
    }
}

/// The camera line of the `list` answer whose `socket=` is `socket`, if the service lists one.
pub fn camera_for_socket(lines: &[String], socket: &Path) -> Option<CameraLine> {
    let wanted = socket.display().to_string();
    lines
        .iter()
        .filter_map(|line| CameraLine::parse(line))
        .find(|cam| cam.get("socket") == Some(wanted.as_str()))
}
