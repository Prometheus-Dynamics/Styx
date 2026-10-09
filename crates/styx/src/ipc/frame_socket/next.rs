//! The `<path>.next` endpoint: a frame the consumer has not seen, sent as soon as there is one.
//!
//! The frame socket itself serves the latest frame to every connection, however often a
//! consumer asks: a consumer that wants each frame once, as it is published, would have to ask
//! again and again (each time getting the frame it has, until a new one is published). Here a
//! consumer says which frame it has (its descriptor `timestamp`, `0` for none) as one line of
//! ASCII decimal, `"<timestamp>\n"`, and the server answers with the first frame published
//! whose timestamp differs from it: at once if the latest is another frame, else when the next
//! one is published. The answer is the frame socket's message, and the connection is its lease,
//! exactly as on the frame socket (any byte or close after the answer releases it). A
//! connection that sends anything else, or more than the line, is closed; one still waiting
//! after [`NEXT_WAIT`] is closed with nothing sent.
//!
//! [`FrameFetcher::fetch_next`](super::FrameFetcher::fetch_next) is the consumer side.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Longest a consumer waits on `<path>.next` for a frame other than the one it has.
pub const NEXT_WAIT: Duration = Duration::from_secs(10);

/// The next-frame endpoint of the frame socket at `path`: `<path>.next`.
pub fn next_path(path: impl AsRef<Path>) -> PathBuf {
    let mut p = path.as_ref().as_os_str().to_owned();
    p.push(".next");
    PathBuf::from(p)
}

/// The request line for a consumer that has the frame with `timestamp` (`0`: none), written
/// into `buf`; the bytes to send.
pub(super) fn request(timestamp: u64, buf: &mut [u8; 24]) -> &[u8] {
    let mut digits = [0u8; 20];
    let mut n = timestamp;
    let mut len = 0;
    loop {
        digits[len] = b'0' + (n % 10) as u8;
        len += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for (i, d) in digits[..len].iter().rev().enumerate() {
        buf[i] = *d;
    }
    buf[len] = b'\n';
    &buf[..=len]
}

/// A consumer on `<path>.next` waiting for its frame.
pub(super) struct Waiter {
    pub(super) socket: UnixStream,
    pub(super) since: Instant,
    /// The timestamp of the frame the consumer has (`0`: none); `None` until its line is read.
    pub(super) after: Option<u64>,
    line: [u8; 24],
    len: usize,
}

impl Waiter {
    pub(super) fn new(socket: std::os::fd::OwnedFd, now: Instant) -> Self {
        Self {
            socket: UnixStream::from(socket),
            since: now,
            after: None,
            line: [0; 24],
            len: 0,
        }
    }

    /// Read what the consumer sent (its socket is readable; it does not block). `false`: the
    /// consumer is gone or broke the protocol (closed before or after its line, sent more than
    /// one line, or something that is not a number): drop it.
    pub(super) fn read(&mut self) -> bool {
        if self.after.is_some() {
            // Its line was read; anything now is a close (or a protocol error).
            return false;
        }
        loop {
            match (&self.socket).read(&mut self.line[self.len..]) {
                Ok(0) => return false,
                Ok(n) => self.len += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
            if let Some(end) = self.line[..self.len].iter().position(|&b| b == b'\n') {
                if end + 1 != self.len {
                    return false;
                }
                self.after = parse(&self.line[..end]);
                return self.after.is_some();
            }
            if self.len == self.line.len() {
                return false;
            }
        }
    }

    /// Whether the frame with `timestamp` is one this consumer has not seen.
    pub(super) fn wants(&self, timestamp: u64) -> bool {
        self.after.is_some_and(|after| after != timestamp)
    }
}

/// ASCII decimal digits (and nothing else) as a `u64`.
fn parse(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u64, |n, &d| {
        d.is_ascii_digit()
            .then(|| n.checked_mul(10)?.checked_add(u64::from(d - b'0')))
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines_parse_back() {
        let mut buf = [0u8; 24];
        for ts in [0, 7, 10, 123_456_789, u64::MAX] {
            let line = request(ts, &mut buf).to_vec();
            assert_eq!(line.last(), Some(&b'\n'));
            assert_eq!(parse(&line[..line.len() - 1]), Some(ts));
            assert_eq!(String::from_utf8(line).unwrap(), format!("{ts}\n"));
        }
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"12a"), None);
        assert_eq!(parse(b"-1"), None);
        assert_eq!(parse(b"18446744073709551616"), None);
    }

    #[test]
    fn a_waiter_reads_one_line() {
        use std::io::Write;
        let (mut consumer, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut w = Waiter::new(server.into(), Instant::now());
        assert!(w.read(), "nothing sent yet: keep waiting");
        consumer.write_all(b"12").unwrap();
        assert!(w.read() && w.after.is_none());
        consumer.write_all(b"34\n").unwrap();
        assert!(w.read());
        assert_eq!(w.after, Some(1234));
        assert!(w.wants(1235) && !w.wants(1234));
        drop(consumer);
        assert!(!w.read(), "a close after the line ends it");

        for bad in [&b"x\n"[..], b"1\n2", b"\n"] {
            let (mut consumer, server) = UnixStream::pair().unwrap();
            server.set_nonblocking(true).unwrap();
            let mut w = Waiter::new(server.into(), Instant::now());
            consumer.write_all(bad).unwrap();
            assert!(!w.read(), "{bad:?} accepted");
        }
    }
}
