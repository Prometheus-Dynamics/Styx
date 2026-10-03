//! Waiting for file descriptors: [`AsyncFd`] and one-off [`readable`]/[`priority`] futures.

use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use super::reactor::{Reactor, Registration};
use super::{Interest, Ready};

/// A file descriptor registered with a [`Reactor`], like tokio's `AsyncFd` but for any
/// executor.
///
/// The descriptor should be non-blocking (see [`super::set_nonblocking`]); the `*_with`
/// helpers call an operation, and wait for readiness whenever it returns `WouldBlock`.
///
/// ```
/// use std::io::{Read, Write};
/// use styx_graph::rt::{self, AsyncFd};
///
/// let (reader, mut writer) = std::io::pipe()?;
/// rt::set_nonblocking(std::os::fd::AsFd::as_fd(&reader))?;
/// let reader = AsyncFd::new(reader)?;
/// writer.write_all(b"hi")?;
/// let mut buf = [0u8; 2];
/// let n = rt::block_on(reader.read_with(|mut r| r.read(&mut buf)))?;
/// assert_eq!(&buf[..n], b"hi");
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct AsyncFd<T: AsFd> {
    inner: Option<T>,
    reg: Arc<Registration>,
}

impl<T: AsFd> AsyncFd<T> {
    /// Register with the global reactor.
    pub fn new(inner: T) -> io::Result<Self> {
        Self::with_reactor(inner, Reactor::try_global()?)
    }

    pub fn with_reactor(inner: T, reactor: &Reactor) -> io::Result<Self> {
        let reg = reactor.register(inner.as_fd().as_raw_fd())?;
        Ok(AsyncFd {
            inner: Some(inner),
            reg,
        })
    }

    pub fn get_ref(&self) -> &T {
        self.inner.as_ref().expect("present until into_inner")
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.as_mut().expect("present until into_inner")
    }

    /// Deregister and return the descriptor.
    pub fn into_inner(mut self) -> T {
        self.reg.deregister();
        self.inner.take().expect("present until into_inner")
    }

    /// Wait until the descriptor reports any of `interest` (or an error/hang-up).
    pub fn ready(&self, interest: Interest) -> Readiness<'_> {
        Readiness::new(self.reg.clone(), interest, None)
    }

    pub fn readable(&self) -> Readiness<'_> {
        self.ready(Interest::READABLE)
    }

    pub fn writable(&self) -> Readiness<'_> {
        self.ready(Interest::WRITABLE)
    }

    /// Wait for priority data: a pending V4L2 event, TCP urgent data.
    pub fn priority(&self) -> Readiness<'_> {
        self.ready(Interest::PRIORITY)
    }

    /// Run `op` until it does not return `WouldBlock`, waiting for `interest` in between.
    pub async fn io_with<R>(
        &self,
        interest: Interest,
        mut op: impl FnMut(&T) -> io::Result<R>,
    ) -> io::Result<R> {
        loop {
            match op(self.get_ref()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.ready(interest).await?;
                }
                result => return result,
            }
        }
    }

    pub async fn read_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        self.io_with(Interest::READABLE, op).await
    }

    pub async fn write_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        self.io_with(Interest::WRITABLE, op).await
    }
}

impl<T: AsFd> AsRawFd for AsyncFd<T> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.get_ref().as_fd().as_raw_fd()
    }
}

impl<T: AsFd> Drop for AsyncFd<T> {
    fn drop(&mut self) {
        // Leave epoll before `inner` closes the descriptor.
        self.reg.deregister();
    }
}

impl<T: AsFd + std::fmt::Debug> std::fmt::Debug for AsyncFd<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncFd")
            .field("inner", &self.inner)
            .finish()
    }
}

/// A future that resolves when a descriptor reports readiness.
///
/// It always reflects the descriptor's current state: if the condition already holds when the
/// future is first polled, it resolves on the reactor's next turn.
#[must_use = "futures do nothing unless polled"]
pub struct Readiness<'a> {
    reg: Arc<Registration>,
    interest: Interest,
    id: u64,
    /// A duplicate of a borrowed descriptor, registered for this wait only.
    temp: Option<OwnedFd>,
    _fd: PhantomData<BorrowedFd<'a>>,
}

impl Readiness<'_> {
    fn new(reg: Arc<Registration>, interest: Interest, temp: Option<OwnedFd>) -> Self {
        Readiness {
            reg,
            interest,
            id: 0,
            temp,
            _fd: PhantomData,
        }
    }

    fn temporary(fd: BorrowedFd<'_>, interest: Interest) -> io::Result<Readiness<'_>> {
        // Register a duplicate so this works even if `fd` is registered elsewhere.
        let dup = fd.try_clone_to_owned()?;
        let reg = Reactor::try_global()?.register(dup.as_raw_fd())?;
        Ok(Readiness::new(reg, interest, Some(dup)))
    }
}

impl Future for Readiness<'_> {
    type Output = io::Result<Ready>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.reg.poll_ready(&mut this.id, this.interest, cx.waker()) {
            Some(result) => Poll::Ready(result),
            None => Poll::Pending,
        }
    }
}

impl Drop for Readiness<'_> {
    fn drop(&mut self) {
        self.reg.cancel(self.id);
        if self.temp.is_some() {
            self.reg.deregister();
        }
    }
}

/// A one-off wait on a borrowed descriptor (registers a duplicate for the duration). Keep an
/// [`AsyncFd`] instead when waiting repeatedly.
fn ready(fd: BorrowedFd<'_>, interest: Interest) -> impl Future<Output = io::Result<Ready>> {
    let wait = Readiness::temporary(fd, interest);
    async move { wait?.await }
}

/// Wait until `fd` is readable (a V4L2 capture buffer can be dequeued).
pub fn readable(fd: BorrowedFd<'_>) -> impl Future<Output = io::Result<Ready>> + '_ {
    ready(fd, Interest::READABLE)
}

/// Wait until `fd` is writable (a V4L2 output buffer can be dequeued for reuse).
pub fn writable(fd: BorrowedFd<'_>) -> impl Future<Output = io::Result<Ready>> + '_ {
    ready(fd, Interest::WRITABLE)
}

/// Wait until `fd` has priority data (a V4L2 event is pending).
pub fn priority(fd: BorrowedFd<'_>) -> impl Future<Output = io::Result<Ready>> + '_ {
    ready(fd, Interest::PRIORITY)
}
