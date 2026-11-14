//! The producer link: connects to a Styx producer, learns its buffers, and reports what it is
//! doing. Pure Rust, and the time is an argument: every method takes `now`, so the backoff is
//! tested with a fake clock. [`Link::drive`] and [`Link::next_frame`] never block on the
//! producer's absence; a lost producer is a status, and the next attempt waits out the backoff
//! (250 ms, doubling, capped at 5 s). The GStreamer source keeps the same state machine in its own
//! crate: the two bridges share no crate below them that both may depend on.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use shm_fdpass as fdpass;
use styx_socket::{Frame, ReadError, Reader};

use crate::learn::{Buffers, Learner};

/// The first delay after a failure.
pub const BACKOFF_START: Duration = Duration::from_millis(250);
/// The longest delay between attempts.
pub const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// Exponential backoff. The delay doubles on each failure up to [`BACKOFF_CAP`]; a success
/// resets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    next: Duration,
    retry_at: Option<Instant>,
}

impl Backoff {
    /// No failure yet: an attempt is due at once.
    pub const fn new() -> Self {
        Self {
            next: BACKOFF_START,
            retry_at: None,
        }
    }

    /// Whether an attempt is due at `now`.
    pub fn due(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|at| now >= at)
    }

    /// Records a failure at `now`. Returns the delay until the next attempt.
    pub fn failed(&mut self, now: Instant) -> Duration {
        let wait = self.next;
        self.retry_at = Some(now + wait);
        self.next = (self.next * 2).min(BACKOFF_CAP);
        wait
    }

    /// Records a success: the next failure starts again at [`BACKOFF_START`].
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// What the link is doing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Trying to connect to the producer.
    #[default]
    Connecting,
    /// Connected; taking the producer's buffer descriptors from its frames.
    Learning,
    /// Buffers learned; frames flow.
    Streaming,
    /// The producer is gone or refused; the next attempt waits out the backoff.
    Waiting,
    /// Stopped by the caller.
    Stopped,
}

/// A snapshot of the link, for the caller.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LinkStatus {
    pub state: LinkState,
    /// Connections made (each one a producer that answered its handshake).
    pub connects: u64,
    /// Failed connects, failed learns and lost connections.
    pub failures: u64,
    /// While waiting: the delay until the next attempt.
    pub next_delay: Option<Duration>,
    /// The last failure's message.
    pub last_error: Option<String>,
}

/// The link to one producer socket.
#[derive(Debug)]
pub struct Link {
    socket: PathBuf,
    reader: Option<Reader>,
    learner: Option<Learner>,
    backoff: Backoff,
    status: LinkStatus,
}

/// What one pass over the learning frames found.
enum Pass {
    Learned,
    Pending,
    /// The producer closed its ring before the buffers were learned: the end of its stream.
    Ended,
    Failed(String),
}

impl Link {
    /// A link to `socket`. Nothing is connected yet.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            reader: None,
            learner: None,
            backoff: Backoff::new(),
            status: LinkStatus::default(),
        }
    }

    /// The producer's socket.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The link's status now.
    pub const fn status(&self) -> &LinkStatus {
        &self.status
    }

    /// Whether the link is connected to a producer whose frames can be read.
    pub const fn streaming(&self) -> bool {
        matches!(self.status.state, LinkState::Streaming)
    }

    /// Sets the state to stopped. The reader is dropped, so the producer counts it gone.
    pub fn stop(&mut self) {
        self.reader = None;
        self.learner = None;
        self.status.state = LinkState::Stopped;
        self.status.next_delay = None;
    }

    /// Connects when an attempt is due at `now`, and learns the buffers from the frames the
    /// producer publishes. Returns the buffers once every one is known; the link then streams.
    pub fn drive(&mut self, now: Instant) -> Option<Buffers> {
        if self.reader.is_none() {
            if !self.backoff.due(now) {
                return None;
            }
            self.status.state = LinkState::Connecting;
            match open(&self.socket) {
                Ok(reader) => {
                    self.learner = Some(Learner::new(reader.geometry().buffers()));
                    self.reader = Some(reader);
                    self.status.connects += 1;
                    self.status.state = LinkState::Learning;
                }
                Err(why) => {
                    self.fail(now, why);
                    return None;
                }
            }
        }
        let reader = self.reader.as_mut()?;
        if !peer_open(reader.socket()) {
            self.fail(now, "the producer closed its socket".into());
            return None;
        }
        let learner = self.learner.as_mut()?;
        match pass(reader, learner) {
            Pass::Pending => None,
            Pass::Ended => {
                self.ended(now);
                None
            }
            Pass::Failed(why) => {
                self.fail(now, why);
                None
            }
            Pass::Learned => {
                let learner = self.learner.take()?;
                match learner.finish() {
                    Ok(buffers) => {
                        self.backoff.reset();
                        self.status.state = LinkState::Streaming;
                        self.status.next_delay = None;
                        Some(buffers)
                    }
                    Err(err) => {
                        self.fail(now, err.to_string());
                        None
                    }
                }
            }
        }
    }

    /// Gives up on the current producer (its format has no PipeWire name, say) and waits out the
    /// backoff before the next attempt.
    pub fn reject(&mut self, now: Instant, why: String) {
        self.fail(now, why);
    }

    /// The next frame of a streaming producer, held. `None` when nothing is new, or the link is
    /// not streaming; a producer that went away is recorded here and retried after the backoff.
    pub fn next_frame(&mut self, now: Instant) -> Option<Frame> {
        if self.learner.is_some() {
            return None;
        }
        let reader = self.reader.as_mut()?;
        if !peer_open(reader.socket()) {
            self.fail(now, "the producer closed its socket".into());
            return None;
        }
        match reader.next_frame() {
            Ok(frame) => frame,
            // The producer closed its ring cleanly: the end of its stream, which is normal.
            Err(ReadError::PublisherClosed) => {
                self.ended(now);
                None
            }
            Err(err) => {
                self.fail(now, err.to_string());
                None
            }
        }
    }

    /// The producer closed its ring cleanly: its stream ended, with every frame delivered. Not a
    /// failure: the reader and the learner are let go, no error is kept, and the next attempt comes
    /// after the first backoff step.
    fn ended(&mut self, now: Instant) {
        self.reader = None;
        self.learner = None;
        self.backoff.reset();
        self.status.state = LinkState::Waiting;
        self.status.next_delay = Some(self.backoff.failed(now));
        self.status.last_error = None;
    }

    fn fail(&mut self, now: Instant, why: String) {
        self.reader = None;
        self.learner = None;
        self.status.failures += 1;
        self.status.state = LinkState::Waiting;
        self.status.next_delay = Some(self.backoff.failed(now));
        self.status.last_error = Some(why);
    }
}

fn open(socket: &Path) -> Result<Reader, String> {
    let stream = UnixStream::connect(socket).map_err(|err| err.to_string())?;
    Reader::connect(stream).map_err(|err| err.to_string())
}

/// Takes what each frame the reader holds gives the learner, releasing each frame at once.
fn pass(reader: &mut Reader, learner: &mut Learner) -> Pass {
    loop {
        match reader.next_frame() {
            Ok(Some(frame)) => match learner.feed(&frame) {
                Ok(true) => return Pass::Learned,
                Ok(false) => {}
                Err(err) => return Pass::Failed(err.to_string()),
            },
            Ok(None) => return Pass::Pending,
            Err(ReadError::PublisherClosed) => return Pass::Ended,
            Err(err) => return Pass::Failed(err.to_string()),
        }
    }
}

/// Whether the producer still holds its end of `socket`. A producer never sends after its
/// handshake, so this only looks for end of stream (the producer makes the same check for its
/// readers). Never blocks.
fn peer_open(socket: &UnixStream) -> bool {
    !matches!(fdpass::recv(socket.as_fd(), &mut [], 0, true), Ok(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_250_ms_to_the_5_s_cap_and_resets() {
        let t0 = Instant::now();
        let mut backoff = Backoff::new();
        let mut delays = Vec::new();
        for _ in 0..7 {
            delays.push(backoff.failed(t0));
        }
        let ms: Vec<u128> = delays.iter().map(Duration::as_millis).collect();
        assert_eq!(ms, [250, 500, 1000, 2000, 4000, 5000, 5000]);
        backoff.reset();
        assert_eq!(backoff.failed(t0), Duration::from_millis(250));
    }

    #[test]
    fn an_attempt_is_due_only_once_the_delay_has_passed() {
        let t0 = Instant::now();
        let mut backoff = Backoff::new();
        assert!(backoff.due(t0));
        backoff.failed(t0);
        assert!(!backoff.due(t0 + Duration::from_millis(249)));
        assert!(backoff.due(t0 + Duration::from_millis(250)));
    }

    #[test]
    fn a_missing_producer_backs_off_on_a_fake_clock_without_sleeping() {
        let t0 = Instant::now();
        let mut link = Link::new("/nonexistent/styx-pw-link-test.sock");
        assert!(link.drive(t0).is_none());
        assert_eq!(link.status().state, LinkState::Waiting);
        assert_eq!(link.status().failures, 1);
        assert_eq!(link.status().next_delay, Some(Duration::from_millis(250)));
        assert!(link.status().last_error.is_some());

        // Inside the window: no attempt, so no new failure.
        assert!(link.drive(t0 + Duration::from_millis(100)).is_none());
        assert_eq!(link.status().failures, 1);

        // Each due attempt fails and doubles the next delay, capped at 5 s.
        let mut now = t0;
        let mut seen = Vec::new();
        for _ in 0..6 {
            let wait = link.status().next_delay.expect("waiting");
            now += wait;
            assert!(link.drive(now).is_none());
            seen.push(link.status().next_delay.expect("waiting").as_millis());
        }
        assert_eq!(seen, [500, 1000, 2000, 4000, 5000, 5000]);
        assert_eq!(link.status().failures, 7);
        assert_eq!(link.status().connects, 0);
    }
}
