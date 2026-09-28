//! The few Linux syscalls the reactor needs: epoll, eventfd, timerfd and `O_NONBLOCK`.
//!
//! This is the only module in the crate with `unsafe`; every block is a direct syscall on file
//! descriptors this module owns or borrows.
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn owned(ret: libc::c_int) -> io::Result<OwnedFd> {
    let fd = check(ret)?;
    // SAFETY: the syscall just returned `fd` as a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) const EPOLLIN: u32 = libc::EPOLLIN as u32;
pub(crate) const EPOLLOUT: u32 = libc::EPOLLOUT as u32;
pub(crate) const EPOLLPRI: u32 = libc::EPOLLPRI as u32;
pub(crate) const EPOLLERR: u32 = libc::EPOLLERR as u32;
pub(crate) const EPOLLHUP: u32 = libc::EPOLLHUP as u32;
pub(crate) const EPOLLRDHUP: u32 = libc::EPOLLRDHUP as u32;
const EPOLLONESHOT: u32 = libc::EPOLLONESHOT as u32;

pub(crate) struct Epoll(OwnedFd);

/// One event from [`Epoll::wait`].
#[derive(Clone, Copy)]
pub(crate) struct Event {
    pub token: u64,
    pub events: u32,
}

impl Epoll {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain syscall without pointers.
        owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) }).map(Epoll)
    }

    fn ctl(&self, op: libc::c_int, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        let mut event = libc::epoll_event { events, u64: token };
        // SAFETY: `event` is a valid epoll_event for the duration of the call; epoll only reads
        // it. A stale `fd` makes the kernel return an error, which we report.
        check(unsafe { libc::epoll_ctl(self.0.as_raw_fd(), op, fd, &mut event) })?;
        Ok(())
    }

    /// Register `fd` level-triggered, one-shot: after one event the fd is disarmed until
    /// [`Epoll::rearm`].
    pub fn add_oneshot(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, events | EPOLLONESHOT, token)
    }

    pub fn rearm(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_MOD, fd, events | EPOLLONESHOT, token)
    }

    /// Register `fd` level-triggered for `events`, permanently.
    pub fn add(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, events, token)
    }

    pub fn delete(&self, fd: RawFd) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_DEL, fd, 0, 0)
    }

    /// Wait for events without a timeout; `EINTR` returns no events.
    pub fn wait(&self, out: &mut Vec<Event>) -> io::Result<()> {
        const MAX: usize = 64;
        let mut raw = [libc::epoll_event { events: 0, u64: 0 }; MAX];
        // SAFETY: `raw` has room for MAX events and outlives the call.
        let n = unsafe { libc::epoll_wait(self.0.as_raw_fd(), raw.as_mut_ptr(), MAX as i32, -1) };
        out.clear();
        if n < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(err)
            };
        }
        out.extend(raw[..n as usize].iter().map(|e| {
            // Copy out of the (packed on x86_64) struct before use.
            let (token, events) = (e.u64, e.events);
            Event { token, events }
        }));
        Ok(())
    }
}

/// An eventfd counter: readable while non-zero.
pub(crate) struct EventFd(OwnedFd);

impl EventFd {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain syscall without pointers.
        owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) }).map(EventFd)
    }

    pub fn notify(&self) -> io::Result<()> {
        write_u64(self.0.as_raw_fd(), 1)
    }

    /// Reset the counter to zero; returns what it was.
    pub fn drain(&self) -> u64 {
        read_u64(self.0.as_raw_fd()).unwrap_or(0)
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsRawFd for EventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl std::os::fd::AsFd for EventFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// A monotonic one-shot timer: readable once it expires.
pub(crate) struct TimerFd(OwnedFd);

impl TimerFd {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain syscall without pointers.
        owned(unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
            )
        })
        .map(TimerFd)
    }

    /// Expire after `after` (at least 1 ns; zero would disarm), or disarm with `None`.
    pub fn set(&self, after: Option<Duration>) -> io::Result<()> {
        let value = match after {
            Some(d) => libc::timespec {
                tv_sec: d.as_secs() as libc::time_t,
                tv_nsec: d.subsec_nanos().max(u32::from(d.as_secs() == 0)) as libc::c_long,
            },
            None => libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        };
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: value,
        };
        // SAFETY: `spec` is valid for the call; the old value pointer may be null.
        check(unsafe {
            libc::timerfd_settime(self.0.as_raw_fd(), 0, &spec, std::ptr::null_mut())
        })?;
        Ok(())
    }

    pub fn drain(&self) {
        let _ = read_u64(self.0.as_raw_fd());
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

fn read_u64(fd: RawFd) -> io::Result<u64> {
    let mut value = 0u64;
    // SAFETY: reads exactly 8 bytes into `value`, which is 8 bytes and lives across the call.
    let n = unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

fn write_u64(fd: RawFd, value: u64) -> io::Result<()> {
    // SAFETY: writes the 8 bytes of `value`, which lives across the call.
    let n = unsafe { libc::write(fd, (&value as *const u64).cast(), 8) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Put `fd` in non-blocking mode, as the reactor's `*_with` helpers expect.
pub fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: F_GETFL/F_SETFL on a borrowed, open descriptor; no pointers involved.
    let flags = check(unsafe { libc::fcntl(raw, libc::F_GETFL) })?;
    // SAFETY: as above.
    check(unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
    Ok(())
}

/// Send one byte of TCP out-of-band data, which makes the peer's socket report `EPOLLPRI`.
#[cfg(test)]
pub(crate) fn send_oob(fd: BorrowedFd<'_>) -> io::Result<()> {
    let byte = b'!';
    // SAFETY: sends one byte from a local that lives across the call.
    let n = unsafe {
        libc::send(
            fd.as_raw_fd(),
            (&byte as *const u8).cast(),
            1,
            libc::MSG_OOB,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eventfd_counts() {
        let efd = EventFd::new().unwrap();
        assert_eq!(efd.drain(), 0);
        efd.notify().unwrap();
        efd.notify().unwrap();
        assert_eq!(efd.drain(), 2);
    }

    #[test]
    fn epoll_reports_oneshot_once() {
        let epoll = Epoll::new().unwrap();
        let efd = EventFd::new().unwrap();
        epoll.add_oneshot(efd.fd(), EPOLLIN, 7).unwrap();
        efd.notify().unwrap();
        let mut events = Vec::new();
        epoll.wait(&mut events).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].token, 7);
        assert_ne!(events[0].events & EPOLLIN, 0);
        epoll.rearm(efd.fd(), EPOLLIN, 8).unwrap();
        epoll.wait(&mut events).unwrap();
        assert_eq!(events[0].token, 8);
        epoll.delete(efd.fd()).unwrap();
        assert!(epoll.delete(efd.fd()).is_err());
    }

    #[test]
    fn timerfd_fires() {
        let epoll = Epoll::new().unwrap();
        let timer = TimerFd::new().unwrap();
        epoll.add(timer.fd(), EPOLLIN, 1).unwrap();
        timer.set(Some(Duration::ZERO)).unwrap();
        let mut events = Vec::new();
        epoll.wait(&mut events).unwrap();
        assert_eq!(events[0].token, 1);
        timer.drain();
        timer.set(None).unwrap();
    }
}
