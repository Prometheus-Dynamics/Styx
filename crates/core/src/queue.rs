//! Bounded queues between a frame's producer and its consumers, `no_std` + `alloc`.
//!
//! A lock-free ring (crossbeam's `ArrayQueue`) with an overflow policy, non-blocking sends and
//! receives, and waker-based async ones ([`BoundedRx::recv_async`], [`BoundedRx::poll_recv`],
//! [`BoundedTx::send_async`]) that run on any executor, or a superloop polling with a no-op
//! waker. With `std`: blocking sends and receives with timeouts. On targets without
//! compare-and-swap (`critical-section`: Cortex-M0+, RISC-V without `a`) the ring is a
//! `VecDeque` in a critical section, with the same behaviour.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

#[cfg(target_has_atomic = "ptr")]
use crossbeam_queue::ArrayQueue;
#[cfg(not(target_has_atomic = "ptr"))]
use ring::ArrayQueue;
use smallvec::SmallVec;

use crate::sync::{Arc, AtomicBool, AtomicU64, Mutex, Ordering};
#[cfg(feature = "std")]
use parking_lot::Condvar;
#[cfg(feature = "std")]
use std::time::{Duration, Instant};

/// Result of attempting to enqueue.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{bounded, RecvOutcome, SendOutcome};
///
/// let (tx, _rx) = bounded::<u8>(1);
/// assert_eq!(tx.send(1), SendOutcome::Ok);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Value was accepted.
    Ok,
    /// Queue is full.
    Full,
    /// Queue is closed.
    Closed,
}

/// Result of attempting to dequeue.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{bounded, RecvOutcome};
///
/// let (_tx, rx) = bounded::<u8>(1);
/// match rx.recv() {
///     RecvOutcome::Empty | RecvOutcome::Closed | RecvOutcome::Data(_) => {}
/// }
/// ```
#[derive(Debug)]
pub enum RecvOutcome<T> {
    /// Received value.
    Data(T),
    /// Queue has been closed and drained.
    Closed,
    /// Queue currently empty.
    Empty,
}

/// Result of waiting for a receive operation.
#[derive(Debug)]
pub enum RecvWaitOutcome<T> {
    /// Received value.
    Data(T),
    /// Queue has been closed and drained.
    Closed,
    /// Timed out while waiting for data.
    Timeout,
}

/// Result of waiting for a send operation.
#[derive(Debug)]
pub enum SendWaitOutcome<T> {
    /// Value was accepted.
    Ok,
    /// Queue has been closed.
    Closed(T),
    /// Timed out while waiting for capacity.
    Timeout(T),
}

/// Snapshot of bounded queue pressure and wait behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStats {
    pub depth: u64,
    pub capacity: u64,
    pub send_backpressure: u64,
    pub send_timeouts: u64,
    /// Values accepted into the queue since it was created.
    pub sent: u64,
    /// Queued values dropped to make room for newer ones ([`QueueOverflow::DropOldest`]).
    pub evictions: u64,
    pub recv_empty: u64,
    pub recv_timeouts: u64,
    pub async_send_waits: u64,
    pub async_recv_waits: u64,
    pub async_send_wakes: u64,
    pub async_recv_wakes: u64,
}

/// Bounded sender handle.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{bounded, RecvOutcome, SendOutcome};
///
/// let (tx, _rx) = bounded::<u8>(1);
/// assert_eq!(tx.send(1), SendOutcome::Ok);
/// ```
pub struct BoundedTx<T> {
    inner: Arc<QueueInner<T>>,
}

// Manual impl: a derive would require `T: Clone`, but senders only share the queue.
impl<T> Clone for BoundedTx<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> BoundedTx<T> {
    /// Attempt to send without blocking.
    pub fn send(&self, value: T) -> SendOutcome {
        // Keep the closed check, push, and wait-state version update ordered so blocking
        // and async receivers cannot miss a wake between checking the queue and parking.
        let state = self.inner.wait_state.lock();
        if self.inner.closed.load(Ordering::Acquire) {
            return SendOutcome::Closed;
        }
        let (outcome, evicted) = match self.inner.push(value) {
            Ok(evicted) => (SendOutcome::Ok, evicted),
            Err(_) => (SendOutcome::Full, None),
        };
        drop(state);
        drop(evicted);
        if matches!(outcome, SendOutcome::Full) {
            self.inner.send_backpressure.fetch_add(1, Ordering::Relaxed);
        }
        if matches!(outcome, SendOutcome::Ok) {
            self.inner.notify_recv_ready();
        }
        outcome
    }

    /// Close the queue to further sends.
    pub fn close(&self) {
        self.inner.close();
    }

    /// Current queue depth.
    pub fn len(&self) -> usize {
        self.inner.queue.len()
    }

    /// Whether the queue currently contains no items.
    pub fn is_empty(&self) -> bool {
        self.inner.queue.is_empty()
    }

    /// Queue capacity.
    pub fn capacity(&self) -> usize {
        self.inner.queue.capacity()
    }

    /// Wait for capacity or closure, with an optional timeout.
    #[cfg(feature = "std")]
    pub fn send_wait(&self, mut value: T, timeout: Option<Duration>) -> SendWaitOutcome<T> {
        let deadline = timeout.map(|wait| Instant::now() + wait);
        loop {
            let mut state = self.inner.wait_state.lock();
            if self.inner.closed.load(Ordering::Acquire) {
                return SendWaitOutcome::Closed(value);
            }
            match self.inner.push(value) {
                Ok(evicted) => {
                    drop(state);
                    drop(evicted);
                    self.inner.notify_recv_ready();
                    return SendWaitOutcome::Ok;
                }
                Err(v) => {
                    value = v;
                }
            }

            let send_version = state.send_version;
            if self.inner.closed.load(Ordering::Acquire) {
                return SendWaitOutcome::Closed(value);
            }

            match deadline {
                Some(deadline) => {
                    if Instant::now() >= deadline {
                        self.inner.send_timeouts.fetch_add(1, Ordering::Relaxed);
                        return SendWaitOutcome::Timeout(value);
                    }
                    let _ = self.inner.send_cv.wait_until(&mut state, deadline);
                    if state.send_version == send_version && Instant::now() >= deadline {
                        self.inner.send_timeouts.fetch_add(1, Ordering::Relaxed);
                        return SendWaitOutcome::Timeout(value);
                    }
                }
                None => {
                    self.inner.send_cv.wait(&mut state);
                }
            }
        }
    }

    /// Wait with a fixed timeout for capacity or closure.
    #[cfg(feature = "std")]
    pub fn send_timeout(&self, value: T, timeout: Duration) -> SendWaitOutcome<T> {
        self.send_wait(value, Some(timeout))
    }

    /// Wait indefinitely for capacity or closure.
    #[cfg(feature = "std")]
    pub fn send_blocking(&self, value: T) -> SendWaitOutcome<T> {
        self.send_wait(value, None)
    }

    /// Snapshot bounded queue stats.
    pub fn stats(&self) -> QueueStats {
        self.inner.stats()
    }
}

impl<T> BoundedTx<T> {
    /// Sends, waiting (asynchronously) for room: `Ok`, or `Closed`. Any executor; the wait
    /// registers the task's waker with the queue.
    pub fn send_async(&self, value: T) -> SendNext<'_, T> {
        SendNext {
            tx: self,
            value: Some(value),
        }
    }

    /// One attempt for [`SendNext`]: pushed, closed, or the waker registered (under the lock
    /// pushes and pops are ordered by, so a pop between the attempt and the registration is
    /// seen) and the value handed back.
    fn poll_send_value(&self, value: T, cx: &mut Context<'_>) -> Result<SendOutcome, T> {
        let mut state = self.inner.wait_state.lock();
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(SendOutcome::Closed);
        }
        match self.inner.push(value) {
            Ok(evicted) => {
                drop(state);
                drop(evicted);
                self.inner.notify_recv_ready();
                Ok(SendOutcome::Ok)
            }
            Err(value) => {
                register(&mut state.send_wakers, cx.waker());
                drop(state);
                self.inner.async_send_waits.fetch_add(1, Ordering::Relaxed);
                Err(value)
            }
        }
    }
}

/// The future of [`BoundedTx::send_async`].
#[must_use = "futures do nothing unless polled"]
pub struct SendNext<'a, T> {
    tx: &'a BoundedTx<T>,
    value: Option<T>,
}

impl<T> Unpin for SendNext<'_, T> {}

impl<T> Future for SendNext<'_, T> {
    type Output = SendOutcome;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SendOutcome> {
        let Some(value) = self.value.take() else {
            return Poll::Ready(SendOutcome::Closed);
        };
        match self.tx.poll_send_value(value, cx) {
            Ok(outcome) => Poll::Ready(outcome),
            Err(value) => {
                self.value = Some(value);
                Poll::Pending
            }
        }
    }
}

/// Bounded receiver handle.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{bounded, RecvOutcome};
///
/// let (_tx, rx) = bounded::<u8>(1);
/// assert!(matches!(rx.recv(), RecvOutcome::Empty | RecvOutcome::Closed));
/// ```
pub struct BoundedRx<T> {
    inner: Arc<QueueInner<T>>,
}

// Manual: another receiver of the same queue, whatever `T` is.
impl<T> Clone for BoundedRx<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> BoundedRx<T> {
    /// Attempt to receive without blocking.
    pub fn recv(&self) -> RecvOutcome<T> {
        match self.inner.queue.pop() {
            Some(value) => {
                self.inner.notify_send_ready();
                RecvOutcome::Data(value)
            }
            None => {
                if self.inner.closed.load(Ordering::Acquire) {
                    RecvOutcome::Closed
                } else {
                    self.inner.recv_empty.fetch_add(1, Ordering::Relaxed);
                    RecvOutcome::Empty
                }
            }
        }
    }

    /// Mark the queue as closed; senders will see `Closed` and exit.
    pub fn close(&self) {
        self.inner.close();
    }

    /// Current queue depth.
    pub fn len(&self) -> usize {
        self.inner.queue.len()
    }

    /// Whether the queue currently contains no items.
    pub fn is_empty(&self) -> bool {
        self.inner.queue.is_empty()
    }

    /// Queue capacity.
    pub fn capacity(&self) -> usize {
        self.inner.queue.capacity()
    }

    /// Wait for data or closure, with an optional timeout.
    #[cfg(feature = "std")]
    pub fn recv_wait(&self, timeout: Option<Duration>) -> RecvWaitOutcome<T> {
        let deadline = timeout.map(|wait| Instant::now() + wait);
        loop {
            let mut state = self.inner.wait_state.lock();
            match self.inner.queue.pop() {
                Some(value) => {
                    drop(state);
                    self.inner.notify_send_ready();
                    return RecvWaitOutcome::Data(value);
                }
                None => {
                    if self.inner.closed.load(Ordering::Acquire) {
                        return RecvWaitOutcome::Closed;
                    }
                    self.inner.recv_empty.fetch_add(1, Ordering::Relaxed);
                }
            }

            let recv_version = state.recv_version;
            if self.inner.closed.load(Ordering::Acquire) && self.inner.queue.is_empty() {
                return RecvWaitOutcome::Closed;
            }

            match deadline {
                Some(deadline) => {
                    if Instant::now() >= deadline {
                        self.inner.recv_timeouts.fetch_add(1, Ordering::Relaxed);
                        return RecvWaitOutcome::Timeout;
                    }
                    let _ = self.inner.recv_cv.wait_until(&mut state, deadline);
                    if state.recv_version == recv_version && Instant::now() >= deadline {
                        self.inner.recv_timeouts.fetch_add(1, Ordering::Relaxed);
                        return RecvWaitOutcome::Timeout;
                    }
                }
                None => {
                    self.inner.recv_cv.wait(&mut state);
                }
            }
        }
    }

    /// Wait with a fixed timeout for data or closure.
    #[cfg(feature = "std")]
    pub fn recv_timeout(&self, timeout: Duration) -> RecvWaitOutcome<T> {
        self.recv_wait(Some(timeout))
    }

    /// Wait indefinitely for data or closure.
    #[cfg(feature = "std")]
    pub fn recv_blocking(&self) -> RecvWaitOutcome<T> {
        self.recv_wait(None)
    }

    /// Snapshot bounded queue stats.
    pub fn stats(&self) -> QueueStats {
        self.inner.stats()
    }
}

impl<T> BoundedRx<T> {
    /// Receives, waiting (asynchronously) for data: `Data`, or `Closed` once closed and
    /// drained. Any executor; a superloop polls [`Self::poll_recv`] with a no-op waker.
    pub fn recv_async(&self) -> RecvNext<'_, T> {
        RecvNext { rx: self }
    }

    /// Data, `Closed` (closed and drained), or `Pending` with `cx`'s waker registered: it is
    /// woken by the next send or by closing.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<RecvOutcome<T>> {
        match self.recv() {
            RecvOutcome::Empty => {}
            other => return Poll::Ready(other),
        }
        let mut state = self.inner.wait_state.lock();
        // Pushes happen under this lock: a send that missed the pop above is seen here, a
        // later one finds the waker.
        if let Some(value) = self.inner.queue.pop() {
            drop(state);
            self.inner.notify_send_ready();
            return Poll::Ready(RecvOutcome::Data(value));
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(RecvOutcome::Closed);
        }
        register(&mut state.recv_wakers, cx.waker());
        drop(state);
        self.inner.async_recv_waits.fetch_add(1, Ordering::Relaxed);
        Poll::Pending
    }
}

/// The future of [`BoundedRx::recv_async`].
#[must_use = "futures do nothing unless polled"]
pub struct RecvNext<'a, T> {
    rx: &'a BoundedRx<T>,
}

impl<T> Future for RecvNext<'_, T> {
    type Output = RecvOutcome<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<RecvOutcome<T>> {
        self.rx.poll_recv(cx)
    }
}

/// Adds `waker` unless it would wake a task already there.
fn register(wakers: &mut Wakers, waker: &Waker) {
    if !wakers.iter().any(|w| w.will_wake(waker)) {
        wakers.push(waker.clone());
    }
}

/// Tasks waiting on one side of a queue (inline for two).
type Wakers = SmallVec<[Waker; 2]>;

/// Wakes and forgets every waiting task.
fn wake_all(wakers: Wakers, wakes: &AtomicU64) {
    wakes.fetch_add(1, Ordering::Relaxed);
    for waker in wakers {
        waker.wake();
    }
}

struct QueueInner<T> {
    queue: ArrayQueue<T>,
    overflow: QueueOverflow,
    closed: AtomicBool,
    send_backpressure: AtomicU64,
    send_timeouts: AtomicU64,
    evictions: AtomicU64,
    sent: AtomicU64,
    recv_empty: AtomicU64,
    recv_timeouts: AtomicU64,
    async_send_waits: AtomicU64,
    async_recv_waits: AtomicU64,
    async_send_wakes: AtomicU64,
    async_recv_wakes: AtomicU64,
    wait_state: Mutex<QueueWaitState>,
    #[cfg(feature = "std")]
    recv_cv: Condvar,
    #[cfg(feature = "std")]
    send_cv: Condvar,
}

struct QueueWaitState {
    recv_version: u64,
    send_version: u64,
    recv_wakers: Wakers,
    send_wakers: Wakers,
}

impl<T> QueueInner<T> {
    /// Push according to the overflow policy; returns the evicted value, if any, so callers
    /// can drop it after releasing the wait-state lock.
    fn push(&self, value: T) -> Result<Option<T>, T> {
        let pushed = match self.overflow {
            QueueOverflow::Backpressure => self.queue.push(value).map(|()| None),
            QueueOverflow::DropOldest => {
                let evicted = self.queue.force_push(value);
                if evicted.is_some() {
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                }
                Ok(evicted)
            }
        };
        if pushed.is_ok() {
            self.sent.fetch_add(1, Ordering::Relaxed);
        }
        pushed
    }

    fn stats(&self) -> QueueStats {
        QueueStats {
            depth: self.queue.len() as u64,
            capacity: self.queue.capacity() as u64,
            send_backpressure: self.send_backpressure.load(Ordering::Relaxed),
            send_timeouts: self.send_timeouts.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            sent: self.sent.load(Ordering::Relaxed),
            recv_empty: self.recv_empty.load(Ordering::Relaxed),
            recv_timeouts: self.recv_timeouts.load(Ordering::Relaxed),
            async_send_waits: self.async_send_waits.load(Ordering::Relaxed),
            async_recv_waits: self.async_recv_waits.load(Ordering::Relaxed),
            async_send_wakes: self.async_send_wakes.load(Ordering::Relaxed),
            async_recv_wakes: self.async_recv_wakes.load(Ordering::Relaxed),
        }
    }

    fn close(&self) {
        let (recv, send) = {
            let mut state = self.wait_state.lock();
            self.closed.store(true, Ordering::Release);
            state.recv_version = state.recv_version.saturating_add(1);
            state.send_version = state.send_version.saturating_add(1);
            (
                core::mem::take(&mut state.recv_wakers),
                core::mem::take(&mut state.send_wakers),
            )
        };
        #[cfg(feature = "std")]
        {
            self.recv_cv.notify_all();
            self.send_cv.notify_all();
        }
        wake_all(recv, &self.async_recv_wakes);
        wake_all(send, &self.async_send_wakes);
    }

    fn notify_recv_ready(&self) {
        let wakers = {
            let mut state = self.wait_state.lock();
            state.recv_version = state.recv_version.saturating_add(1);
            (!state.recv_wakers.is_empty()).then(|| core::mem::take(&mut state.recv_wakers))
        };
        #[cfg(feature = "std")]
        self.recv_cv.notify_all();
        if let Some(wakers) = wakers {
            wake_all(wakers, &self.async_recv_wakes);
        }
    }

    fn notify_send_ready(&self) {
        let wakers = {
            let mut state = self.wait_state.lock();
            state.send_version = state.send_version.saturating_add(1);
            (!state.send_wakers.is_empty()).then(|| core::mem::take(&mut state.send_wakers))
        };
        #[cfg(feature = "std")]
        self.send_cv.notify_all();
        if let Some(wakers) = wakers {
            wake_all(wakers, &self.async_send_wakes);
        }
    }
}

/// Create a bounded queue with the given capacity.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{bounded, RecvOutcome, SendOutcome};
///
/// let (tx, rx) = bounded::<u8>(1);
/// assert_eq!(tx.send(1), SendOutcome::Ok);
/// match rx.recv() {
///     RecvOutcome::Data(_) | RecvOutcome::Empty | RecvOutcome::Closed => {}
/// }
/// ```
pub fn bounded<T>(capacity: usize) -> (BoundedTx<T>, BoundedRx<T>) {
    bounded_with(capacity, QueueOverflow::Backpressure)
}

/// What a bounded queue does when a value is sent while it is full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum QueueOverflow {
    /// Refuse the value: `send` returns `Full`, `send_wait` waits for room or times out.
    #[default]
    Backpressure,
    /// Accept the value and drop the oldest queued one, so receivers always get the newest
    /// values. Sends never block or time out.
    DropOldest,
}

/// Create a bounded queue with an explicit overflow policy.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{QueueOverflow, RecvOutcome, bounded_with};
///
/// let (tx, rx) = bounded_with::<u8>(1, QueueOverflow::DropOldest);
/// let _ = tx.send(1);
/// let _ = tx.send(2);
/// assert!(matches!(rx.recv(), RecvOutcome::Data(2)));
/// assert_eq!(tx.stats().evictions, 1);
/// ```
pub fn bounded_with<T>(capacity: usize, overflow: QueueOverflow) -> (BoundedTx<T>, BoundedRx<T>) {
    let inner = Arc::new(QueueInner {
        queue: ArrayQueue::new(capacity),
        overflow,
        closed: AtomicBool::new(false),
        send_backpressure: AtomicU64::new(0),
        send_timeouts: AtomicU64::new(0),
        evictions: AtomicU64::new(0),
        sent: AtomicU64::new(0),
        recv_empty: AtomicU64::new(0),
        recv_timeouts: AtomicU64::new(0),
        async_send_waits: AtomicU64::new(0),
        async_recv_waits: AtomicU64::new(0),
        async_send_wakes: AtomicU64::new(0),
        async_recv_wakes: AtomicU64::new(0),
        wait_state: Mutex::new(QueueWaitState {
            recv_version: 0,
            send_version: 0,
            recv_wakers: Wakers::new(),
            send_wakers: Wakers::new(),
        }),
        #[cfg(feature = "std")]
        recv_cv: Condvar::new(),
        #[cfg(feature = "std")]
        send_cv: Condvar::new(),
    });
    (
        BoundedTx {
            inner: inner.clone(),
        },
        BoundedRx { inner },
    )
}

/// Default capacity used by [`default_bounded`].
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// Create a bounded queue using [`DEFAULT_QUEUE_CAPACITY`].
///
/// # Example
/// ```rust
/// use styx_core::prelude::{DEFAULT_QUEUE_CAPACITY, default_bounded};
///
/// let (tx, _rx) = default_bounded::<u8>();
/// assert_eq!(tx.capacity(), DEFAULT_QUEUE_CAPACITY);
/// ```
pub fn default_bounded<T>() -> (BoundedTx<T>, BoundedRx<T>) {
    bounded(DEFAULT_QUEUE_CAPACITY)
}

mod newest;
pub use newest::{NewestRx, NewestTx, newest};
#[cfg(any(test, not(target_has_atomic = "ptr")))]
mod ring;

#[cfg(all(test, feature = "std"))]
mod async_tests;
#[cfg(all(test, feature = "std"))]
mod tests;
#[cfg(test)]
mod waker_tests;
