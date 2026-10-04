//! Shared state and counters, one answer per build.
//!
//! With `std` a camera's parts run on threads (the Linux event thread, a consumer, a pipeline
//! thread): [`Shared<T>`] is `Arc<Mutex<T>>` and [`Ref<T>`] is `Arc<T>`, exactly what the Linux
//! runtime locked with before. Without it they run in one task (a microcontroller camera;
//! interrupt handlers touch only the receiver's own atomics and wakers): `Rc<RefCell<T>>` and
//! `Rc<T>`, so no lock or critical section is ever held across an I²C transfer. A cargo
//! feature, not a generic: one binary has one answer.

/// A counter of events: 64-bit where the target has 64-bit atomics, else 32-bit (it wraps
/// after 2^32 events; frame counters at 60 fps wrap after two years).
#[derive(Debug, Default)]
pub struct Counter(Atomic);

#[cfg(target_has_atomic = "64")]
type Atomic = core::sync::atomic::AtomicU64;
#[cfg(not(target_has_atomic = "64"))]
type Atomic = core::sync::atomic::AtomicU32;

impl Counter {
    /// A counter at zero.
    pub const fn new() -> Self {
        Self(Atomic::new(0))
    }

    /// Adds `n`.
    #[inline]
    pub fn add(&self, n: u64) {
        #[allow(clippy::cast_possible_truncation, clippy::useless_conversion)]
        self.0
            .fetch_add(n as _, core::sync::atomic::Ordering::Relaxed);
    }

    /// Adds one.
    #[inline]
    pub fn incr(&self) {
        self.add(1);
    }

    /// The count.
    #[inline]
    pub fn get(&self) -> u64 {
        #[allow(clippy::useless_conversion)]
        u64::from(self.0.load(core::sync::atomic::Ordering::Relaxed))
    }
}

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
    pub type Shared<T> = alloc::rc::Rc<core::cell::RefCell<T>>;
    /// The lock inside a [`Shared`].
    pub type Lock<T> = core::cell::RefCell<T>;
    /// A locked [`Lock`].
    pub type Guard<'a, T> = core::cell::RefMut<'a, T>;
    /// A shared reference (internally synchronised state).
    pub type Ref<T> = alloc::rc::Rc<T>;

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
