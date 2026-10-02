//! The fake sensor bridge (see `fake.rs`): stream requests queued on start/stop with a bounded
//! wait for the acknowledgement (`ETIMEDOUT`, `ESTALE` for late acks), power refused while not
//! idle, and unplugging (`ENODEV`, hang-up).

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use styx_kernel::Wait;
use styx_kernel::bus::{StreamAction, StreamRequest, StreamState};

use crate::device::BridgeDevice;
use crate::fake::{Signal, errno, lock};
use crate::regbus::PowerSwitch;

/// Bridge stream states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BridgeState {
    Idle,
    Starting,
    Streaming,
    Stopping,
    StartFailed,
}

#[derive(Debug)]
struct BridgeInner {
    state: BridgeState,
    power: bool,
    seq: u32,
    /// The sequence the bridge waits for, and the ack status once given.
    waiting: Option<u32>,
    status: Option<i32>,
    pending: VecDeque<StreamRequest>,
    timeout: Duration,
    gone: bool,
    /// Requests are not queued (lost): starts time out.
    deaf: bool,
    /// What a request carries besides action, sequence and timeout.
    template: StreamRequest,
    /// Acks that came too late (`ESTALE`).
    stale_acks: u64,
    /// Requests that timed out.
    timeouts: u64,
    /// Failed starts go to the receiver (`report_start_errors=1`); by default they are only
    /// reported in the state.
    report_errors: bool,
}

/// The sensor bridge.
pub(crate) struct FakeBridge {
    inner: Mutex<BridgeInner>,
    acked: Condvar,
    signal: Signal,
}

impl FakeBridge {
    pub(crate) fn new(template: StreamRequest) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BridgeInner {
                state: BridgeState::Idle,
                power: false,
                seq: 0,
                waiting: None,
                status: None,
                pending: VecDeque::new(),
                timeout: Duration::from_millis(500),
                gone: false,
                deaf: false,
                template,
                stale_acks: 0,
                timeouts: 0,
                report_errors: false,
            }),
            acked: Condvar::new(),
            signal: Signal::new(),
        })
    }

    pub(crate) fn state(&self) -> BridgeState {
        lock(&self.inner).state
    }

    pub(crate) fn powered(&self) -> bool {
        lock(&self.inner).power
    }

    pub(crate) fn stale_acks(&self) -> u64 {
        lock(&self.inner).stale_acks
    }

    /// Requests nobody acknowledged in time.
    pub(crate) fn timeouts(&self) -> u64 {
        lock(&self.inner).timeouts
    }

    pub(crate) fn set_timeout(&self, t: Duration) {
        lock(&self.inner).timeout = t;
    }

    /// Return failed starts to the receiver (the module's `report_start_errors=1`).
    pub(crate) fn set_report_errors(&self, on: bool) {
        lock(&self.inner).report_errors = on;
    }

    /// Requests get lost from now on (`true`), or are delivered again.
    pub(crate) fn set_deaf(&self, deaf: bool) {
        lock(&self.inner).deaf = deaf;
    }

    /// The module is unloaded / the overlay removed: every call fails with `ENODEV`, waits
    /// end with it, the node hangs up.
    pub(crate) fn unplug(&self) {
        let mut b = lock(&self.inner);
        b.gone = true;
        b.power = false;
        b.state = BridgeState::Idle;
        if b.waiting.take().is_some() {
            b.status = Some(-libc::ENODEV);
        }
        drop(b);
        self.acked.notify_all();
        self.signal.hang_up();
    }

    /// The receiver's `s_stream`: queues a request and waits for its acknowledgement.
    pub(crate) fn s_stream(&self, on: bool) -> io::Result<()> {
        let mut b = lock(&self.inner);
        if b.gone {
            return Err(errno(libc::ENODEV));
        }
        if !on && b.state == BridgeState::StartFailed {
            b.state = BridgeState::Idle;
            return Ok(());
        }
        let (action, transient) = if on {
            if b.state != BridgeState::Idle {
                return Err(errno(libc::EBUSY));
            }
            (StreamAction::Start, BridgeState::Starting)
        } else {
            if b.state != BridgeState::Streaming {
                return Ok(());
            }
            (StreamAction::Stop, BridgeState::Stopping)
        };
        b.seq += 1;
        let seq = b.seq;
        b.waiting = Some(seq);
        b.status = None;
        b.state = transient;
        let req = StreamRequest {
            action,
            sequence: seq,
            timeout: b.timeout,
            ..b.template
        };
        if !b.deaf {
            b.pending.push_back(req);
            self.signal.raise();
        }
        let deadline = Instant::now() + b.timeout;
        while b.status.is_none() && !b.gone {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            b = self
                .acked
                .wait_timeout(b, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        b.waiting = None;
        if b.status.is_none() && !b.gone {
            b.timeouts += 1;
        }
        let status = b.status.take().unwrap_or(-libc::ETIMEDOUT);
        if b.gone {
            return Err(errno(libc::ENODEV));
        }
        b.state = match (on, status) {
            (true, 0) => BridgeState::Streaming,
            (true, _) if !b.report_errors => BridgeState::StartFailed,
            _ => BridgeState::Idle,
        };
        if on && status != 0 && b.report_errors {
            return Err(errno(-status));
        }
        Ok(())
    }
}

impl AsFd for FakeBridge {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.signal.reader.as_fd()
    }
}

impl BridgeDevice for FakeBridge {
    fn try_next_request(&self) -> io::Result<Option<StreamRequest>> {
        let mut b = lock(&self.inner);
        if b.gone {
            return Err(errno(libc::ENODEV));
        }
        let r = b.pending.pop_front();
        if r.is_some() {
            self.signal.lower();
        }
        Ok(r)
    }

    fn acknowledge(&self, req: &StreamRequest, result: Result<(), i32>) -> io::Result<()> {
        let mut b = lock(&self.inner);
        if b.gone {
            return Err(errno(libc::ENODEV));
        }
        if b.waiting != Some(req.sequence) || b.status.is_some() {
            b.stale_acks += 1;
            return Err(errno(libc::ESTALE));
        }
        b.status = Some(result.map_or_else(|e| -e, |()| 0));
        drop(b);
        self.acked.notify_all();
        Ok(())
    }

    fn power(&self) -> io::Result<bool> {
        let b = lock(&self.inner);
        if b.gone {
            return Err(errno(libc::ENODEV));
        }
        Ok(b.power)
    }

    fn set_power(&self, on: bool) -> io::Result<()> {
        let mut b = lock(&self.inner);
        if b.gone {
            return Err(errno(libc::ENODEV));
        }
        if !on && b.state != BridgeState::Idle {
            return Err(errno(libc::EBUSY));
        }
        b.power = on;
        Ok(())
    }

    fn stream_state(&self) -> io::Result<StreamState> {
        let b = lock(&self.inner);
        if b.gone {
            return Err(errno(libc::ENODEV));
        }
        Ok(match b.state {
            BridgeState::Idle => StreamState::Idle,
            BridgeState::Starting => StreamState::Starting,
            BridgeState::Streaming => StreamState::Streaming,
            BridgeState::Stopping => StreamState::Stopping,
            BridgeState::StartFailed => StreamState::StartFailed,
        })
    }

    fn request_wait(&self) -> Wait {
        Wait::READABLE
    }
}

impl PowerSwitch for FakeBridge {
    fn set_power(&self, on: bool) -> io::Result<()> {
        BridgeDevice::set_power(self, on)
    }
}
