use crossbeam_queue::ArrayQueue;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
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
        self.inner
            .queue
            .push(value)
            .map(|_| SendOutcome::Ok)
            .unwrap_or(SendOutcome::Full)
    }

    /// Close the queue to further sends.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
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
                Ok(()) => return SendOutcome::Ok,
                Err(v) => {
                    value = v;
                    tokio::task::yield_now().await;
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
            Some(value) => RecvOutcome::Data(value),
            None => {
                if self.inner.closed.load(Ordering::Acquire) {
                    RecvOutcome::Closed
                } else {
                    RecvOutcome::Empty
                }
            }
        }
    }

    /// Mark the queue as closed; senders will see `Closed` and exit.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
    }
}

#[cfg(feature = "async")]
impl<T> BoundedRx<T> {
    /// Async helper that yields until data or closure.
    pub async fn recv_async(&self) -> RecvOutcome<T> {
        loop {
            match self.recv() {
                RecvOutcome::Empty => {
                    tokio::task::yield_now().await;
                }
                other => return other,
            }
        }
    }
}

struct QueueInner<T> {
    queue: ArrayQueue<T>,
    closed: AtomicBool,
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
