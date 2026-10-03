//! A small runtime-agnostic async core: readiness of file descriptors, timers, `block_on`.
//!
//! Futures here work on any executor (tokio, smol/async-io, a hand-written loop) or none: they
//! register with a [`Reactor`] thread that waits in `epoll` and wakes their [`std::task::Waker`].
//! V4L2 and media devices are pollable, so a capture queue is "readable" when a buffer can be
//! dequeued, "writable" when an output buffer can be queued, and has "priority" data when a
//! V4L2 event (e.g. from the sensor bridge) is pending.
//!
//! ```
//! use std::io::Write;
//! use std::time::Duration;
//! use styx_graph::rt::{self, AsyncFd};
//!
//! let (reader, mut writer) = std::io::pipe()?;
//! let reader = AsyncFd::new(reader)?;
//! writer.write_all(b"frame")?;
//! let ready = rt::block_on(rt::timeout(Duration::from_secs(1), reader.readable()));
//! assert!(ready.expect("in time")?.is_readable());
//! # Ok::<(), std::io::Error>(())
//! ```

mod block_on;
mod io;
mod reactor;
mod stream;
mod sys;
mod time;

#[cfg(test)]
mod tests;

pub use block_on::block_on;
pub use io::{AsyncFd, Readiness, priority, readable, writable};
pub use reactor::Reactor;
pub use stream::{Next, next};
pub use sys::set_nonblocking;
pub use time::{Elapsed, Sleep, Timeout, sleep, sleep_until, timeout};

use std::fmt;
use std::ops::BitOr;

/// What to wait for on a descriptor. Combine with `|`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interest(u8);

impl Interest {
    /// Data to read (`EPOLLIN`): a V4L2 capture buffer is ready to dequeue.
    pub const READABLE: Interest = Interest(1);
    /// Room to write (`EPOLLOUT`): a V4L2 output buffer can be dequeued for reuse.
    pub const WRITABLE: Interest = Interest(1 << 1);
    /// Priority/exceptional data (`EPOLLPRI`): a V4L2 event is pending.
    pub const PRIORITY: Interest = Interest(1 << 2);

    pub const fn contains(self, other: Interest) -> bool {
        self.0 & other.0 == other.0
    }

    fn epoll_bits(self) -> u32 {
        let mut bits = 0;
        if self.contains(Self::READABLE) {
            bits |= sys::EPOLLIN | sys::EPOLLRDHUP;
        }
        if self.contains(Self::WRITABLE) {
            bits |= sys::EPOLLOUT;
        }
        if self.contains(Self::PRIORITY) {
            bits |= sys::EPOLLPRI;
        }
        bits
    }
}

impl BitOr for Interest {
    type Output = Interest;
    fn bitor(self, rhs: Self) -> Self {
        Interest(self.0 | rhs.0)
    }
}

impl fmt::Debug for Interest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Interest({:?})", Ready(self.0))
    }
}

/// What a descriptor reported. Errors and hang-ups are reported to every waiter, whatever it
/// asked for, so the next I/O call can surface them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Ready(u8);

impl Ready {
    pub const EMPTY: Ready = Ready(0);
    pub const READABLE: Ready = Ready(1);
    pub const WRITABLE: Ready = Ready(1 << 1);
    pub const PRIORITY: Ready = Ready(1 << 2);
    pub const ERROR: Ready = Ready(1 << 3);
    /// The peer closed (`EPOLLHUP`/`EPOLLRDHUP`), or a V4L2 device was unplugged.
    pub const HANGUP: Ready = Ready(1 << 4);

    fn from_epoll(bits: u32) -> Ready {
        let mut ready = 0;
        if bits & sys::EPOLLIN != 0 {
            ready |= Self::READABLE.0;
        }
        if bits & sys::EPOLLOUT != 0 {
            ready |= Self::WRITABLE.0;
        }
        if bits & sys::EPOLLPRI != 0 {
            ready |= Self::PRIORITY.0;
        }
        if bits & sys::EPOLLERR != 0 {
            ready |= Self::ERROR.0;
        }
        if bits & (sys::EPOLLHUP | sys::EPOLLRDHUP) != 0 {
            ready |= Self::HANGUP.0;
        }
        Ready(ready)
    }

    /// The part of this readiness that answers `interest` (always including errors/hang-ups).
    fn matching(self, interest: Interest) -> Ready {
        Ready(self.0 & (interest.0 | Self::ERROR.0 | Self::HANGUP.0))
    }

    pub const fn contains(self, other: Ready) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn is_readable(self) -> bool {
        self.contains(Self::READABLE)
    }

    pub const fn is_writable(self) -> bool {
        self.contains(Self::WRITABLE)
    }

    pub const fn is_priority(self) -> bool {
        self.contains(Self::PRIORITY)
    }

    pub const fn is_error(self) -> bool {
        self.contains(Self::ERROR)
    }

    pub const fn is_hangup(self) -> bool {
        self.contains(Self::HANGUP)
    }
}

impl BitOr for Ready {
    type Output = Ready;
    fn bitor(self, rhs: Self) -> Self {
        Ready(self.0 | rhs.0)
    }
}

impl fmt::Debug for Ready {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [
            (Self::READABLE, "READABLE"),
            (Self::WRITABLE, "WRITABLE"),
            (Self::PRIORITY, "PRIORITY"),
            (Self::ERROR, "ERROR"),
            (Self::HANGUP, "HANGUP"),
        ];
        let set: Vec<&str> = names
            .iter()
            .filter(|(r, _)| self.contains(*r))
            .map(|(_, n)| *n)
            .collect();
        write!(f, "{}", set.join(" | "))
    }
}
