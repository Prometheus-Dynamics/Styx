//! A small unbounded async channel (for hotplug events), independent of any runtime.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use futures_core::Stream;

struct Shared<T> {
    queue: VecDeque<T>,
    waker: Option<Waker>,
    senders: usize,
    receiver: bool,
}

fn lock<T>(shared: &Mutex<Shared<T>>) -> MutexGuard<'_, Shared<T>> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Sender<T>(Arc<Mutex<Shared<T>>>);
pub(crate) struct Receiver<T>(Arc<Mutex<Shared<T>>>);

pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Mutex::new(Shared {
        queue: VecDeque::new(),
        waker: None,
        senders: 1,
        receiver: true,
    }));
    (Sender(shared.clone()), Receiver(shared))
}

impl<T> Sender<T> {
    /// Queue `value`; false once the receiver is gone.
    pub fn send(&self, value: T) -> bool {
        let mut shared = lock(&self.0);
        if !shared.receiver {
            return false;
        }
        shared.queue.push_back(value);
        let waker = shared.waker.take();
        drop(shared);
        if let Some(w) = waker {
            w.wake();
        }
        true
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        lock(&self.0).senders += 1;
        Sender(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut shared = lock(&self.0);
        shared.senders -= 1;
        let waker = if shared.senders == 0 {
            shared.waker.take()
        } else {
            None
        };
        drop(shared);
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut shared = lock(&self.0);
        if let Some(value) = shared.queue.pop_front() {
            return Poll::Ready(Some(value));
        }
        if shared.senders == 0 {
            return Poll::Ready(None);
        }
        shared.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut shared = lock(&self.0);
        shared.receiver = false;
        shared.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rt::{block_on, next};

    #[test]
    fn delivers_in_order_and_ends() {
        let (tx, mut rx) = channel();
        let tx2 = tx.clone();
        assert!(tx.send(1));
        assert!(tx2.send(2));
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            tx2.send(3);
        });
        drop(tx);
        assert_eq!(block_on(next(&mut rx)), Some(1));
        assert_eq!(block_on(next(&mut rx)), Some(2));
        assert_eq!(block_on(next(&mut rx)), Some(3));
        t.join().unwrap();
        assert_eq!(block_on(next(&mut rx)), None);
    }

    #[test]
    fn send_fails_without_receiver() {
        let (tx, rx) = channel::<u8>();
        drop(rx);
        assert!(!tx.send(1));
    }
}
