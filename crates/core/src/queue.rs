use crossbeam_queue::ArrayQueue;
use parking_lot::{Condvar, Mutex};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

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
    pub recv_empty: u64,
    pub recv_timeouts: u64,
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
#[derive(Clone)]
pub struct BoundedTx<T> {
    inner: Arc<QueueInner<T>>,
}

impl<T> BoundedTx<T> {
    /// Attempt to send without blocking.
    pub fn send(&self, value: T) -> SendOutcome {
        if self.inner.closed.load(Ordering::Acquire) {
            return SendOutcome::Closed;
        }
        let outcome = self
            .inner
            .queue
            .push(value)
            .map(|_| SendOutcome::Ok)
            .unwrap_or(SendOutcome::Full);
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
        self.inner.closed.store(true, Ordering::Release);
        self.inner.notify_closed();
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
    pub fn send_wait(&self, mut value: T, timeout: Option<Duration>) -> SendWaitOutcome<T> {
        let deadline = timeout.map(|wait| Instant::now() + wait);
        loop {
            if self.inner.closed.load(Ordering::Acquire) {
                return SendWaitOutcome::Closed(value);
            }
            match self.inner.queue.push(value) {
                Ok(()) => {
                    self.inner.notify_recv_ready();
                    return SendWaitOutcome::Ok;
                }
                Err(v) => {
                    value = v;
                }
            }

            let mut state = self.inner.wait_state.lock();
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
    pub fn send_timeout(&self, value: T, timeout: Duration) -> SendWaitOutcome<T> {
        self.send_wait(value, Some(timeout))
    }

    /// Wait indefinitely for capacity or closure.
    pub fn send_blocking(&self, value: T) -> SendWaitOutcome<T> {
        self.send_wait(value, None)
    }

    /// Snapshot bounded queue stats.
    pub fn stats(&self) -> QueueStats {
        self.inner.stats()
    }
}

#[cfg(feature = "async")]
impl<T> BoundedTx<T> {
    /// Async helper that yields on backpressure.
    pub async fn send_async(&self, mut value: T) -> SendOutcome {
        loop {
            if self.inner.closed.load(Ordering::Acquire) {
                return SendOutcome::Closed;
            }
            match self.inner.queue.push(value) {
                Ok(()) => {
                    self.inner.notify_recv_ready();
                    return SendOutcome::Ok;
                }
                Err(v) => {
                    value = v;
                    self.inner.send_notify.notified().await;
                }
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
#[derive(Clone)]
pub struct BoundedRx<T> {
    inner: Arc<QueueInner<T>>,
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
        self.inner.closed.store(true, Ordering::Release);
        self.inner.notify_closed();
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
    pub fn recv_wait(&self, timeout: Option<Duration>) -> RecvWaitOutcome<T> {
        let deadline = timeout.map(|wait| Instant::now() + wait);
        loop {
            match self.recv() {
                RecvOutcome::Data(value) => return RecvWaitOutcome::Data(value),
                RecvOutcome::Closed => return RecvWaitOutcome::Closed,
                RecvOutcome::Empty => {}
            }

            let mut state = self.inner.wait_state.lock();
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
    pub fn recv_timeout(&self, timeout: Duration) -> RecvWaitOutcome<T> {
        self.recv_wait(Some(timeout))
    }

    /// Wait indefinitely for data or closure.
    pub fn recv_blocking(&self) -> RecvWaitOutcome<T> {
        self.recv_wait(None)
    }

    /// Snapshot bounded queue stats.
    pub fn stats(&self) -> QueueStats {
        self.inner.stats()
    }
}

#[cfg(feature = "async")]
impl<T> BoundedRx<T> {
    /// Async helper that waits until data or closure.
    pub async fn recv_async(&self) -> RecvOutcome<T> {
        loop {
            match self.recv() {
                RecvOutcome::Empty => self.inner.recv_notify.notified().await,
                other => return other,
            }
        }
    }
}

struct QueueInner<T> {
    queue: ArrayQueue<T>,
    closed: AtomicBool,
    send_backpressure: AtomicU64,
    send_timeouts: AtomicU64,
    recv_empty: AtomicU64,
    recv_timeouts: AtomicU64,
    wait_state: Mutex<QueueWaitState>,
    recv_cv: Condvar,
    send_cv: Condvar,
    #[cfg(feature = "async")]
    recv_notify: tokio::sync::Notify,
    #[cfg(feature = "async")]
    send_notify: tokio::sync::Notify,
}

struct QueueWaitState {
    recv_version: u64,
    send_version: u64,
}

impl<T> QueueInner<T> {
    fn stats(&self) -> QueueStats {
        QueueStats {
            depth: self.queue.len() as u64,
            capacity: self.queue.capacity() as u64,
            send_backpressure: self.send_backpressure.load(Ordering::Relaxed),
            send_timeouts: self.send_timeouts.load(Ordering::Relaxed),
            recv_empty: self.recv_empty.load(Ordering::Relaxed),
            recv_timeouts: self.recv_timeouts.load(Ordering::Relaxed),
        }
    }

    fn notify_recv_ready(&self) {
        {
            let mut state = self.wait_state.lock();
            state.recv_version = state.recv_version.saturating_add(1);
        }
        self.recv_cv.notify_all();
        #[cfg(feature = "async")]
        self.recv_notify.notify_waiters();
    }

    fn notify_send_ready(&self) {
        {
            let mut state = self.wait_state.lock();
            state.send_version = state.send_version.saturating_add(1);
        }
        self.send_cv.notify_all();
        #[cfg(feature = "async")]
        self.send_notify.notify_waiters();
    }

    fn notify_closed(&self) {
        {
            let mut state = self.wait_state.lock();
            state.recv_version = state.recv_version.saturating_add(1);
            state.send_version = state.send_version.saturating_add(1);
        }
        self.recv_cv.notify_all();
        self.send_cv.notify_all();
        #[cfg(feature = "async")]
        {
            self.recv_notify.notify_waiters();
            self.send_notify.notify_waiters();
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
    let inner = Arc::new(QueueInner {
        queue: ArrayQueue::new(capacity),
        closed: AtomicBool::new(false),
        send_backpressure: AtomicU64::new(0),
        send_timeouts: AtomicU64::new(0),
        recv_empty: AtomicU64::new(0),
        recv_timeouts: AtomicU64::new(0),
        wait_state: Mutex::new(QueueWaitState {
            recv_version: 0,
            send_version: 0,
        }),
        recv_cv: Condvar::new(),
        send_cv: Condvar::new(),
        #[cfg(feature = "async")]
        recv_notify: tokio::sync::Notify::new(),
        #[cfg(feature = "async")]
        send_notify: tokio::sync::Notify::new(),
    });
    (
        BoundedTx {
            inner: inner.clone(),
        },
        BoundedRx { inner },
    )
}

/// Create an unbounded queue using a generous default capacity.
///
/// # Example
/// ```rust
/// use styx_core::prelude::unbounded;
///
/// let (_tx, _rx) = unbounded::<u8>();
/// ```
pub fn unbounded<T>() -> (BoundedTx<T>, BoundedRx<T>) {
    bounded(1024)
}

/// Newest-value queue: always returns the latest value without backpressure.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{newest, RecvOutcome};
///
/// let (tx, rx) = newest::<u8>();
/// let _ = tx.send(5);
/// assert!(matches!(rx.recv(), RecvOutcome::Data(_)));
/// ```
pub fn newest<T>() -> (NewestTx<T>, NewestRx<T>)
where
    T: Clone,
{
    let shared = Arc::new(NewestInner {
        slot: parking_lot::RwLock::new(None),
        closed: AtomicBool::new(false),
    });
    (
        NewestTx {
            inner: shared.clone(),
        },
        NewestRx { inner: shared },
    )
}

/// Sender for newest-value queue.
///
/// # Example
/// ```rust
/// use styx_core::prelude::newest;
///
/// let (tx, _rx) = newest::<u8>();
/// let _ = tx.send(1);
/// ```
#[derive(Clone)]
pub struct NewestTx<T> {
    inner: Arc<NewestInner<T>>,
}

impl<T: Clone> NewestTx<T> {
    /// Overwrite with the latest value.
    pub fn send(&self, value: T) -> SendOutcome {
        if self.inner.closed.load(Ordering::Acquire) {
            return SendOutcome::Closed;
        }
        *self.inner.slot.write() = Some(value);
        SendOutcome::Ok
    }

    /// Close the queue.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
    }
}

/// Receiver for newest-value queue.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{newest, RecvOutcome};
///
/// let (_tx, rx) = newest::<u8>();
/// assert!(matches!(rx.recv(), RecvOutcome::Empty | RecvOutcome::Closed));
/// ```
#[derive(Clone)]
pub struct NewestRx<T> {
    inner: Arc<NewestInner<T>>,
}

impl<T: Clone> NewestRx<T> {
    /// Get the latest value if present.
    pub fn recv(&self) -> RecvOutcome<T> {
        let read = self.inner.slot.read();
        if let Some(value) = read.as_ref() {
            RecvOutcome::Data(value.clone())
        } else if self.inner.closed.load(Ordering::Acquire) {
            RecvOutcome::Closed
        } else {
            RecvOutcome::Empty
        }
    }
}

struct NewestInner<T> {
    slot: parking_lot::RwLock<Option<T>>,
    closed: AtomicBool,
}

#[cfg(test)]
mod tests {
    use super::{RecvWaitOutcome, SendWaitOutcome, bounded};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn send_timeout_returns_value_when_queue_stays_full() {
        let (tx, rx) = bounded(1);
        assert!(matches!(tx.send(1), super::SendOutcome::Ok));

        match tx.send_timeout(2, Duration::from_millis(5)) {
            SendWaitOutcome::Timeout(value) => assert_eq!(value, 2),
            other => panic!("expected timeout, got {other:?}"),
        }

        match rx.recv() {
            super::RecvOutcome::Data(value) => assert_eq!(value, 1),
            other => panic!("expected queued value, got {other:?}"),
        }
    }

    #[test]
    fn send_timeout_wakes_when_receiver_makes_capacity() {
        let (tx, rx) = bounded(1);
        assert!(matches!(tx.send(1), super::SendOutcome::Ok));

        let rx_worker = rx.clone();
        let join = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            match rx_worker.recv_blocking() {
                RecvWaitOutcome::Data(value) => assert_eq!(value, 1),
                other => panic!("expected first queued value, got {other:?}"),
            }
        });

        assert!(matches!(
            tx.send_timeout(2, Duration::from_millis(50)),
            SendWaitOutcome::Ok
        ));

        join.join().expect("receiver thread");
        match rx.recv() {
            super::RecvOutcome::Data(value) => assert_eq!(value, 2),
            other => panic!("expected second queued value, got {other:?}"),
        }
    }
}
