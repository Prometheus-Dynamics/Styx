//! The grouper's descriptor: an epoll set holding an eventfd (woken by in-process queues
//! through a [`Waker`] that writes it), a timerfd (the next group deadline) and the descriptors
//! of sources that have one (camera service clients). Readable exactly when
//! [`FrameGrouper::try_event`](super::FrameGrouper::try_event) has something to do.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, OnceLock};
use std::task::{Wake, Waker};
use std::time::Duration;

use styx_graph::rt::AsyncFd;

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn owned(ret: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a non-negative result of the creating syscall is a new descriptor we own.
    check(ret).map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Wakes the grouper's descriptor: writes its eventfd.
struct FdWaker(OwnedFd);

impl FdWaker {
    fn notify(&self) {
        let one = 1u64;
        // SAFETY: writes the 8 bytes of `one` to a non-blocking eventfd.
        let _ = unsafe { libc::write(self.0.as_raw_fd(), (&raw const one).cast(), 8) };
    }
}

impl Wake for FdWaker {
    fn wake(self: Arc<Self>) {
        self.notify();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notify();
    }
}

pub(super) struct Poller {
    epoll: OwnedFd,
    events: Arc<FdWaker>,
    timer: OwnedFd,
    waker: Waker,
    reactor: OnceLock<io::Result<AsyncFd<OwnedFd>>>,
}

impl Poller {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: plain syscalls without pointers.
        let epoll = owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        // SAFETY: as above.
        let events = owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
        // SAFETY: as above.
        let timer = owned(unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
            )
        })?;
        let events = Arc::new(FdWaker(events));
        let poller = Self {
            epoll,
            waker: Waker::from(events.clone()),
            events,
            timer,
            reactor: OnceLock::new(),
        };
        poller.ctl(libc::EPOLL_CTL_ADD, poller.events.0.as_raw_fd())?;
        poller.ctl(libc::EPOLL_CTL_ADD, poller.timer.as_raw_fd())?;
        Ok(poller)
    }

    fn ctl(&self, op: libc::c_int, fd: RawFd) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: fd as u64,
        };
        // SAFETY: valid epoll and target descriptors, and a valid event.
        check(unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), op, fd, &mut event) }).map(drop)
    }

    /// Wake when `fd` is readable (a source's own descriptor).
    pub(super) fn watch(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        match self.ctl(libc::EPOLL_CTL_ADD, fd.as_raw_fd()) {
            Err(err) if err.raw_os_error() == Some(libc::EEXIST) => Ok(()),
            other => other,
        }
    }

    pub(super) fn unwatch(&self, fd: BorrowedFd<'_>) {
        let _ = self.ctl(libc::EPOLL_CTL_DEL, fd.as_raw_fd());
    }

    /// The waker in-process sources register: it makes the descriptor readable.
    pub(super) fn waker(&self) -> &Waker {
        &self.waker
    }

    /// Readable again at once (more to do than one turn took).
    pub(super) fn notify(&self) {
        self.events.notify();
    }

    /// Not readable for the eventfd and the timer any more (call before looking at sources,
    /// so a wake landing meanwhile is kept).
    pub(super) fn clear(&self) {
        let mut value = 0u64;
        for fd in [self.events.0.as_raw_fd(), self.timer.as_raw_fd()] {
            // SAFETY: reads 8 bytes into a u64 from a non-blocking eventfd or timerfd.
            let _ = unsafe { libc::read(fd, (&raw mut value).cast(), 8) };
        }
    }

    /// Readable after `after` (`None`: disarm).
    pub(super) fn wake_after(&self, after: Option<Duration>) {
        let ts = |d: Duration| libc::timespec {
            tv_sec: d.as_secs() as _,
            tv_nsec: libc::c_long::from(d.subsec_nanos() as i32),
        };
        let value = after.map_or(Duration::ZERO, |d| d.max(Duration::from_nanos(1)));
        let spec = libc::itimerspec {
            it_interval: ts(Duration::ZERO),
            it_value: ts(value),
        };
        // SAFETY: a valid timerfd and timer value; the old value is not asked for.
        let _ = unsafe {
            libc::timerfd_settime(self.timer.as_raw_fd(), 0, &spec, std::ptr::null_mut())
        };
    }

    /// Wait up to `wait` for the descriptor to be readable.
    pub(super) fn wait(&self, wait: Duration) {
        let mut poll = libc::pollfd {
            fd: self.epoll.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Round up: a sub-millisecond deadline must not turn into a busy loop.
        let ms = wait.as_nanos().div_ceil(1_000_000);
        let timeout = i32::try_from(ms).unwrap_or(i32::MAX);
        // SAFETY: one valid `pollfd`.
        let _ = unsafe { libc::poll(&mut poll, 1, timeout) };
    }

    /// The descriptor registered with styx-graph's reactor (a duplicate), on first use.
    pub(super) fn reactor(&self) -> io::Result<&AsyncFd<OwnedFd>> {
        self.reactor
            .get_or_init(|| AsyncFd::new(self.epoll.try_clone()?))
            .as_ref()
            .map_err(|err| io::Error::new(err.kind(), err.to_string()))
    }

    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.epoll.as_fd()
    }
}
