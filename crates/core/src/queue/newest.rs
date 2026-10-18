//! The newest-value queue: a slot overwritten by each send.

use super::{RecvOutcome, SendOutcome};
use crate::sync::{Arc, AtomicBool, Ordering, RwLock};

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
        slot: RwLock::new(None),
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
        // The previous value is dropped after the write lock is released: dropping it can take
        // other locks (a `FrameLease` returns its buffer to a pool).
        let previous = self.inner.slot.write().replace(value);
        drop(previous);
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
    ///
    /// The value is cloned while the read lock is held, so a concurrent `send` waits for the
    /// clone. Cloning outside the lock would need the slot to hold an `Arc<T>` (one extra
    /// allocation per send), so the clone stays under the lock.
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
    slot: RwLock<Option<T>>,
    closed: AtomicBool,
}
