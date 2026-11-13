//! The block link a decoder runs its jobs on: the session, opened on demand, and the recovery
//! after a failure. Shared by the HEVC and H.264 decoders.
//!
//! Recovery: a session that fails (a [`CodecError::Failed`] or [`CodecError::Unavailable`]) is
//! closed, and the next call after a backoff opens it again. The backoff starts at 250 ms and
//! doubles to 8 s, and the link never gives up and never turns itself off. A job the block
//! refuses ([`CodecError::Rejected`]) leaves the session open and sets no backoff.
//!
//! Provenance: moved from Lemnos `drivers/m2m-decode` (`decoder.rs`, lock 04d09360).

use std::time::Instant;

use styx_codec_api::CodecError;

use crate::decoder::Session;

/// The first backoff after a failed session, in nanoseconds.
pub const BACKOFF_FIRST_NS: u64 = 250_000_000;

/// The backoff before reopening a session that failed `attempt` times in a row: 250 ms, doubling
/// to 8 s and no longer.
pub const fn backoff_ns(attempt: u32) -> u64 {
    BACKOFF_FIRST_NS << if attempt > 5 { 5 } else { attempt }
}

/// A session opener. Called at the first job, and again after each failure once its backoff has
/// passed.
pub type Open = Box<dyn FnMut() -> Result<Box<dyn Session>, CodecError> + Send>;

/// The block link: the session, its failures, and when it may be opened again.
pub struct Link {
    open: Open,
    session: Option<Box<dyn Session>>,
    started: Instant,
    failures: u32,
    retry_at_ns: Option<u64>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("open", &self.is_open())
            .field("failures", &self.failures)
            .field("retry_at_ns", &self.retry_at_ns)
            .finish()
    }
}

impl Link {
    /// A link over the sessions `open` opens. Nothing is opened until the first job.
    pub fn new(open: Open) -> Self {
        Self {
            open,
            session: None,
            started: Instant::now(),
            failures: 0,
            retry_at_ns: None,
        }
    }

    /// Whether a session is open.
    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// Failed sessions in a row. 0 after a job completes.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    fn now_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Closes the session after a failure and schedules its reopening.
    fn fail(&mut self, now_ns: u64) {
        self.session = None;
        self.retry_at_ns = Some(now_ns.saturating_add(backoff_ns(self.failures)));
        self.failures = self.failures.saturating_add(1);
    }

    /// The session, opened if it is not, unless its backoff has not passed.
    fn session(&mut self, now_ns: u64) -> Result<&mut Box<dyn Session>, CodecError> {
        if let Some(at) = self.retry_at_ns
            && now_ns < at
        {
            return Err(CodecError::Unavailable(format!(
                "the decoder failed: it is opened again in {} ms",
                (at - now_ns) / 1_000_000
            )));
        }
        if self.session.is_none() {
            match (self.open)() {
                Ok(session) => {
                    self.session = Some(session);
                    self.retry_at_ns = None;
                }
                Err(error) => {
                    self.fail(now_ns);
                    return Err(error);
                }
            }
        }
        Ok(self.session.as_mut().expect("opened above"))
    }

    /// Runs one job's work on the session, opening it first when it is not open. Records a
    /// failure for the backoff, and leaves a refusal to the caller.
    pub fn run(&mut self, work: &[u8]) -> Result<(), CodecError> {
        let now = self.now_ns();
        let session = self.session(now)?;
        match session.run(work) {
            Ok(_) => {
                self.failures = 0;
                Ok(())
            }
            Err(error @ CodecError::Rejected(_)) => Err(error),
            Err(error) => {
                let now = self.now_ns();
                self.fail(now);
                Err(error)
            }
        }
    }
}
