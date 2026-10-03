//! Timers: [`sleep`], [`sleep_until`] and [`timeout`], driven by the reactor's timerfd.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use super::reactor::Reactor;

/// A future that completes at a deadline. It can be [reset](Sleep::reset) to a new deadline,
/// which makes it cheap to use as a periodic tick.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct Sleep {
    deadline: Instant,
    reactor: &'static Reactor,
    key: Option<u64>,
}

/// Complete after `duration`, on the global reactor.
pub fn sleep(duration: Duration) -> Sleep {
    sleep_until(Instant::now() + duration)
}

/// Complete at `deadline`, on the global reactor.
pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep {
        deadline,
        reactor: Reactor::global(),
        key: None,
    }
}

impl Sleep {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn is_elapsed(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Wait for `deadline` instead; the future can be polled again after completing.
    pub fn reset(&mut self, deadline: Instant) {
        self.cancel();
        self.deadline = deadline;
    }

    fn cancel(&mut self) {
        if let Some(key) = self.key.take() {
            self.reactor.cancel_timer(self.deadline, key);
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if Instant::now() >= this.deadline {
            this.cancel();
            return Poll::Ready(());
        }
        let registered = this
            .key
            .is_some_and(|key| this.reactor.update_timer(this.deadline, key, cx.waker()));
        if !registered {
            this.key = Some(this.reactor.add_timer(this.deadline, cx.waker().clone()));
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The deadline of a [`timeout`] passed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("deadline elapsed")]
pub struct Elapsed;

impl From<Elapsed> for std::io::Error {
    fn from(_: Elapsed) -> Self {
        std::io::Error::new(std::io::ErrorKind::TimedOut, Elapsed)
    }
}

/// Run `future` for at most `duration`.
pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    Timeout {
        future: Box::pin(future),
        sleep: sleep(duration),
    }
}

/// The future of [`timeout`]. The wrapped future is boxed so this crate needs no unsafe pin
/// projection.
#[must_use = "futures do nothing unless polled"]
pub struct Timeout<F> {
    future: Pin<Box<F>>,
    sleep: Sleep,
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Poll::Ready(output) = this.future.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }
        match Pin::new(&mut this.sleep).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<F> std::fmt::Debug for Timeout<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timeout")
            .field("sleep", &self.sleep)
            .finish()
    }
}
