//! A `next()` for [`futures_core::Stream`], so callers need no extra crate.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

/// The next item of `stream` (`None` when it ended).
pub fn next<S: Stream + Unpin + ?Sized>(stream: &mut S) -> Next<'_, S> {
    Next { stream }
}

#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct Next<'a, S: ?Sized> {
    stream: &'a mut S,
}

impl<S: Stream + Unpin + ?Sized> Future for Next<'_, S> {
    type Output = Option<S::Item>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut *self.get_mut().stream).poll_next(cx)
    }
}
