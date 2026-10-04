//! Shared state and counters, one answer per build.
//!
//! With `std` a camera's parts run on threads (the Linux event thread, a consumer, a pipeline
//! thread): [`Shared<T>`] is `Arc<Mutex<T>>` and [`Ref<T>`] is `Arc<T>`, exactly what the Linux
//! runtime locked with before. Without it they run in one task (a microcontroller camera;
//! interrupt handlers touch only the receiver's own atomics and wakers): `Arc<RefCell<T>>` and
//! `Arc<T>`, so no lock or critical section is ever held across an I²C transfer. `Arc` (not
//! `Rc`) so that what is `Sync` (a receiver, the frame pool) can be shared: a frame handed on as
//! a `FrameLease` is `Send` on every target; `Arc<RefCell<T>>` itself is not, so the sensor
//! state still cannot leave its task. A cargo feature, not a generic: one binary has one
//! answer.

/// A 64-bit counter of events, relaxed (`styx_core::sync::Counter`).
pub use styx_core::sync::Counter;

#[cfg(feature = "std")]
mod imp {
    /// State shared between a camera's parts.
    pub type Shared<T> = std::sync::Arc<std::sync::Mutex<T>>;
    /// The lock inside a [`Shared`].
    pub type Lock<T> = std::sync::Mutex<T>;
    /// A locked [`Lock`].
    pub type Guard<'a, T> = std::sync::MutexGuard<'a, T>;
    /// A shared reference (internally synchronised state).
    pub type Ref<T> = std::sync::Arc<T>;

    /// Locks, recovering from a poisoned lock (the state is plain data).
    #[inline]
    pub fn lock<T>(m: &Lock<T>) -> Guard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new lock.
    pub fn new_lock<T>(value: T) -> Lock<T> {
        std::sync::Mutex::new(value)
    }
}

#[cfg(not(feature = "std"))]
mod imp {
    /// State shared between a camera's parts.
    pub type Shared<T> = alloc::sync::Arc<core::cell::RefCell<T>>;
    /// The lock inside a [`Shared`].
    pub type Lock<T> = core::cell::RefCell<T>;
    /// A locked [`Lock`].
    pub type Guard<'a, T> = core::cell::RefMut<'a, T>;
    /// A shared reference (internally synchronised state).
    pub type Ref<T> = alloc::sync::Arc<T>;

    /// Borrows mutably (the parts run in one task: a nested borrow is a bug and panics).
    #[inline]
    pub fn lock<T>(m: &Lock<T>) -> Guard<'_, T> {
        m.borrow_mut()
    }

    /// A new lock.
    pub fn new_lock<T>(value: T) -> Lock<T> {
        core::cell::RefCell::new(value)
    }
}

pub use imp::{Guard, Lock, Ref, Shared, lock, new_lock};

/// Shares `value`.
pub fn shared<T>(value: T) -> Shared<T> {
    Ref::new(new_lock(value))
}

/// `Send` with `std` (the handle crosses threads), nothing without.
#[cfg(feature = "std")]
pub trait MaybeSend: Send {}
#[cfg(feature = "std")]
impl<T: Send + ?Sized> MaybeSend for T {}
/// `Send` with `std` (the handle crosses threads), nothing without.
#[cfg(not(feature = "std"))]
pub trait MaybeSend {}
#[cfg(not(feature = "std"))]
impl<T: ?Sized> MaybeSend for T {}
