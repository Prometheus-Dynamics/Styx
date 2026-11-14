//! The producer's state page: whether the camera is running or retrying, since when, and how it
//! came back. One sealed page per producer, shared read-only with every reader in the handshake
//! (the offer's third descriptor). The words mirror the hardware layer's producer state (I2h), so
//! a reader learns one thing whichever ring it reads.
//!
//! Words (u64, every access atomic): 0 reserved (zero); 1 seq, a seqlock: odd while the producer
//! writes words 2 to 5, even when they are stable; 2 edges, the number of state changes this
//! producer ever made; 3 state, 0 running and 1 retrying (other values pass through as
//! [`ProducerState::Unknown`]); 4 since_ns, the CLOCK_MONOTONIC time of the edge to retrying (0
//! while running); 5 recovery, set at each edge back to running: 0 kept (the same driver instance
//! came back), 1 lost (the device was rebound: a new instance). Other values read as lost, the
//! conservative reading: a sequence discontinuity.

use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;

use shm_ring::{Mem, MemMut, ReadMap, WriteMap, read_only, sealed_memfd};

use crate::SocketError;

/// The state page's size in bytes. The memory is rounded up to a page.
const PAGE_BYTES: usize = 64;
const W_SEQ: usize = 1;
const W_EDGES: usize = 2;
const W_STATE: usize = 3;
const W_SINCE: usize = 4;
const W_RECOVERY: usize = 5;
/// How many times a reader retries a seqlock read the producer keeps interrupting. A producer
/// writes for a few stores; this bound only stops a corrupt page from holding a reader forever.
const SNAPSHOT_TRIES: usize = 64;

/// What the camera producing a stream is doing now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerState {
    /// The camera runs.
    Running,
    /// The camera failed transiently and is retrying: the stream stays attached. `since_ns` is
    /// the CLOCK_MONOTONIC time of the failure, in nanoseconds.
    Retrying {
        /// When the retry began, CLOCK_MONOTONIC nanoseconds.
        since_ns: u64,
    },
    /// Another state value, passed through and never read as running.
    Unknown(u32),
}

/// How a camera came back from a stall (I2h's `Carried`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carried {
    /// The same driver instance recovered: the frames continue, possibly with a gap.
    Kept,
    /// The device was rebound: a new instance. The frame sequence restarts; treat it as a
    /// discontinuity, not a gap count.
    Lost,
}

impl Carried {
    const fn code(self) -> u64 {
        match self {
            Self::Kept => 0,
            Self::Lost => 1,
        }
    }

    const fn from_code(code: u64) -> Self {
        match code {
            0 => Self::Kept,
            _ => Self::Lost,
        }
    }
}

/// The producer's side: the page it writes, and the state it last wrote.
#[derive(Debug)]
pub(crate) struct StateWriter {
    map: WriteMap,
    state: ProducerState,
    recovery: Carried,
    edges: u64,
}

impl StateWriter {
    /// A new page, running, with no edges.
    pub(crate) fn create() -> Result<Self, SocketError> {
        let map = WriteMap::new(sealed_memfd(PAGE_BYTES)?)?;
        let writer = Self {
            map,
            state: ProducerState::Running,
            recovery: Carried::Kept,
            edges: 0,
        };
        writer.publish(ProducerState::Running, Carried::Kept, 0);
        Ok(writer)
    }

    /// A read-only descriptor for the same page, for a reader's offer.
    pub(crate) fn reader_fd(&self) -> Result<OwnedFd, SocketError> {
        Ok(read_only(self.map.fd())?)
    }

    pub(crate) const fn state(&self) -> ProducerState {
        self.state
    }

    /// How the last recovery went (kept until the next edge back to running).
    pub(crate) const fn recovery(&self) -> Carried {
        self.recovery
    }

    /// Records a change of state, with the recovery an edge back to running carries. Returns
    /// `false` (and writes nothing) when the state does not change: the recovery of the last edge
    /// stays until the next one.
    pub(crate) fn change(&mut self, state: ProducerState, recovery: Carried) -> bool {
        if state == self.state {
            return false;
        }
        self.edges += 1;
        self.state = state;
        self.recovery = recovery;
        let since = match state {
            ProducerState::Retrying { since_ns } => since_ns,
            _ => 0,
        };
        self.publish(state, recovery, since);
        true
    }

    /// The seqlock write: odd while the words change, even when stable.
    fn publish(&self, state: ProducerState, recovery: Carried, since_ns: u64) {
        let seq = self.map.load(W_SEQ, Ordering::Relaxed);
        self.map.store(W_SEQ, seq | 1, Ordering::Relaxed);
        <WriteMap as Mem>::fence(Ordering::Release);
        let word = match state {
            ProducerState::Running => 0,
            ProducerState::Retrying { .. } => 1,
            ProducerState::Unknown(code) => u64::from(code),
        };
        self.map.store(W_EDGES, self.edges, Ordering::Relaxed);
        self.map.store(W_STATE, word, Ordering::Relaxed);
        self.map.store(W_SINCE, since_ns, Ordering::Relaxed);
        self.map.store(W_RECOVERY, recovery.code(), Ordering::Relaxed);
        self.map.store(W_SEQ, (seq | 1).wrapping_add(1), Ordering::Release);
    }
}

/// A reader's view of the page: read-only, stable snapshots only.
#[derive(Debug)]
pub(crate) struct StateReader {
    map: ReadMap,
}

/// One stable reading of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) state: ProducerState,
    pub(crate) recovery: Carried,
    pub(crate) edges: u64,
}

impl Snapshot {
    /// Whether the producer runs.
    pub(crate) const fn running(self) -> bool {
        matches!(self.state, ProducerState::Running)
    }
}

impl StateReader {
    pub(crate) fn new(fd: OwnedFd) -> Result<Self, SocketError> {
        Ok(Self {
            map: ReadMap::new(fd)?,
        })
    }

    /// A stable reading: the same even sequence before and after the words are read.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let mut last = Snapshot {
            state: ProducerState::Running,
            recovery: Carried::Kept,
            edges: 0,
        };
        for _ in 0..SNAPSHOT_TRIES {
            let before = self.map.load(W_SEQ, Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let word = self.map.load(W_STATE, Ordering::Relaxed);
            let since_ns = self.map.load(W_SINCE, Ordering::Relaxed);
            let recovery = self.map.load(W_RECOVERY, Ordering::Relaxed);
            let edges = self.map.load(W_EDGES, Ordering::Relaxed);
            <ReadMap as Mem>::fence(Ordering::Acquire);
            let after = self.map.load(W_SEQ, Ordering::Relaxed);
            last = Snapshot {
                state: match word {
                    0 => ProducerState::Running,
                    1 => ProducerState::Retrying { since_ns },
                    other => ProducerState::Unknown(other as u32),
                },
                recovery: Carried::from_code(recovery),
                edges,
            };
            if before == after {
                return last;
            }
        }
        last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_state_word_reads_back_unknown_and_never_running() {
        let writer = StateWriter::create().expect("a state page");
        let reader = StateReader::new(writer.reader_fd().expect("fd")).expect("map");
        assert_eq!(reader.snapshot().state, ProducerState::Running);
        writer.map.store(W_STATE, 7, Ordering::Relaxed);
        assert_eq!(reader.snapshot().state, ProducerState::Unknown(7));
        assert!(!reader.snapshot().running());
    }

    #[test]
    fn carried_codes_round_trip_and_unknown_recovery_reads_as_lost() {
        assert_eq!(Carried::from_code(Carried::Kept.code()), Carried::Kept);
        assert_eq!(Carried::from_code(Carried::Lost.code()), Carried::Lost);
        assert_eq!(Carried::from_code(9), Carried::Lost);
    }

    #[test]
    fn each_change_counts_one_edge_and_no_change_counts_none() {
        let mut writer = StateWriter::create().expect("a state page");
        let reader = StateReader::new(writer.reader_fd().expect("fd")).expect("map");
        assert!(writer.change(ProducerState::Retrying { since_ns: 5 }, Carried::Kept));
        assert!(!writer.change(ProducerState::Retrying { since_ns: 5 }, Carried::Kept));
        assert!(writer.change(ProducerState::Running, Carried::Lost));
        let snap = reader.snapshot();
        assert_eq!(snap.edges, 2);
        assert_eq!(snap.state, ProducerState::Running);
        assert_eq!(snap.recovery, Carried::Lost);
    }
}
