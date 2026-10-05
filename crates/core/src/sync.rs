//! Shared ownership, atomics and locks for the frame path, one answer per build.
//!
//! | Build | [`Arc`] / [`Weak`] | Atomics, [`fence`], [`compiler_fence`] | Locks (internal) |
//! |---|---|---|---|
//! | `std` | `alloc::sync`'s (std's) | `portable-atomic` (core's instructions where the target has them) | parking_lot |
//! | no `std` | `alloc::sync`'s | `portable-atomic` (64-bit: a lock-based fallback on 32-bit MCUs) | spin locks |
//! | no `std`, `critical-section` | `alloc::sync`'s, or `portable-atomic-util`'s where the target has no compare-and-swap | through `critical-section` | critical sections |
//!
//! Every integer width (8 to 64 bits, signed and unsigned, and pointer-sized), [`AtomicBool`]
//! and [`AtomicPtr`] are here, with core's API (`portable-atomic` mirrors it, adding the
//! read-modify-write operations everywhere). Code that takes all of them from this module
//! builds unchanged on every target Styx builds for, Cortex-M0 included. Where the target has
//! the instructions the types are core's in all but name: the same code, no lock.
//!
//! Locks are only held for a few instructions (a free list push or pop, a waker list) and
//! never across anything that waits. A spin lock is right for tasks and superloops; a
//! firmware that touches a pool or a queue from an interrupt handler enables
//! `critical-section`, so the interrupt cannot land while the task holds the lock.

#[cfg(target_has_atomic = "ptr")]
pub use alloc::sync::{Arc, Weak};
#[cfg(not(target_has_atomic = "ptr"))]
pub use portable_atomic_util::{Arc, Weak};

pub use portable_atomic::{
    AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicPtr, AtomicU8,
    AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, compiler_fence, fence,
};

#[cfg(all(not(target_has_atomic = "ptr"), not(feature = "critical-section")))]
compile_error!(
    "styx-core on a target without compare-and-swap needs the `critical-section` feature \
     (and a critical-section implementation from the platform)"
);

#[cfg(feature = "std")]
mod imp {
    pub(crate) type Mutex<T> = parking_lot::Mutex<T>;
    pub(crate) type RwLock<T> = parking_lot::RwLock<T>;
}

#[cfg(all(not(feature = "std"), not(feature = "critical-section")))]
mod imp {
    pub(crate) type Mutex<T> = spin::mutex::SpinMutex<T>;
    pub(crate) type RwLock<T> = spin::RwLock<T>;
}

#[cfg(all(not(feature = "std"), feature = "critical-section"))]
mod imp {
    use core::cell::{Cell, UnsafeCell};
    use core::ops::{Deref, DerefMut};

    /// A lock that holds a critical section (interrupts off on a single-core MCU) while it is
    /// held. Locking it again while held (a bug: the holder never waits) panics.
    pub(crate) struct Mutex<T> {
        held: Cell<bool>,
        data: UnsafeCell<T>,
    }

    // SAFETY: the data is only reached through a guard, which exists only inside a critical
    // section with `held` set: one guard at a time across every context.
    unsafe impl<T: Send> Sync for Mutex<T> {}
    unsafe impl<T: Send> Send for Mutex<T> {}

    impl<T> Mutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self {
                held: Cell::new(false),
                data: UnsafeCell::new(value),
            }
        }

        pub(crate) fn lock(&self) -> Guard<'_, T> {
            // SAFETY: released exactly once, by the guard's drop, in reverse order of nesting
            // (guards are scoped and not `Send`).
            let restore = unsafe { critical_section::acquire() };
            assert!(!self.held.replace(true), "styx-core lock taken twice");
            Guard {
                lock: self,
                restore,
                _not_send: core::marker::PhantomData,
            }
        }

        // The queue (the one reader-writer user) needs compare-and-swap.
        #[cfg_attr(not(target_has_atomic = "ptr"), allow(dead_code))]
        pub(crate) fn read(&self) -> Guard<'_, T> {
            self.lock()
        }

        #[cfg_attr(not(target_has_atomic = "ptr"), allow(dead_code))]
        pub(crate) fn write(&self) -> Guard<'_, T> {
            self.lock()
        }
    }

    pub(crate) struct Guard<'a, T> {
        lock: &'a Mutex<T>,
        restore: critical_section::RestoreState,
        _not_send: core::marker::PhantomData<*const ()>,
    }

    impl<T> Deref for Guard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // SAFETY: the guard is the only access (see `Mutex`).
            unsafe { &*self.lock.data.get() }
        }
    }

    impl<T> DerefMut for Guard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: as above, and `&mut self` makes it unique.
            unsafe { &mut *self.lock.data.get() }
        }
    }

    impl<T> Drop for Guard<'_, T> {
        fn drop(&mut self) {
            self.lock.held.set(false);
            // SAFETY: the state `acquire` returned in `lock`, released once.
            unsafe { critical_section::release(self.restore) };
        }
    }

    /// Readers and writers alike take the critical section.
    #[cfg_attr(not(target_has_atomic = "ptr"), allow(dead_code))]
    pub(crate) type RwLock<T> = Mutex<T>;
}

pub(crate) use imp::Mutex;
#[cfg(target_has_atomic = "ptr")]
pub(crate) use imp::RwLock;

/// A 64-bit counter of events, relaxed: native atomics where the target has 64-bit ones (the
/// same instructions as `core`'s), `portable-atomic`'s fallback where it does not (Cortex-M,
/// 32-bit RISC-V).
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// A counter at zero.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Adds `n`.
    #[inline]
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// Adds `n`, returning the count before.
    #[inline]
    pub fn fetch_add(&self, n: u64) -> u64 {
        self.0.fetch_add(n, Ordering::Relaxed)
    }

    /// Adds one.
    #[inline]
    pub fn incr(&self) {
        self.add(1);
    }

    /// Subtracts `n` (wrapping).
    #[inline]
    pub fn sub(&self, n: u64) {
        self.0.fetch_sub(n, Ordering::Relaxed);
    }

    /// The count.
    #[inline]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Sets the count.
    #[inline]
    pub fn set(&self, n: u64) {
        self.0.store(n, Ordering::Relaxed);
    }

    /// Raises the count to `n` if it is lower (a high-water mark).
    #[inline]
    pub fn max(&self, n: u64) {
        self.0.fetch_max(n, Ordering::Relaxed);
    }
}

impl Clone for Counter {
    fn clone(&self) -> Self {
        Self(AtomicU64::new(self.get()))
    }
}

// Every exported atomic and its read-modify-write operations, named once outside the tests: the
// builds for each target (`scripts/check-nostd.sh`: Cortex-M4F/M7/M33, RISC-V, wasm, and with
// `critical-section` Cortex-M0 and RISC-V without atomics) check the whole set exists there.
// Never called.
const _: fn() = || {
    let o = Ordering::Relaxed;
    let _ = AtomicBool::new(false).fetch_xor(true, o);
    let _ = AtomicI8::new(0).fetch_add(1, o);
    let _ = AtomicI16::new(0).fetch_sub(1, o);
    let _ = AtomicI32::new(0).fetch_max(1, o);
    let _ = AtomicI64::new(0).fetch_min(1, o);
    let _ = AtomicIsize::new(0).swap(1, o);
    let _ = AtomicU8::new(0).fetch_or(1, o);
    let _ = AtomicU16::new(0).fetch_and(1, o);
    let _ = AtomicU32::new(0).compare_exchange(0, 1, o, o);
    let _ = AtomicU64::new(0).fetch_update(o, o, |v| Some(v + 1));
    let _ = AtomicUsize::new(0).fetch_add(1, o);
    let _ = AtomicPtr::<u8>::new(core::ptr::null_mut()).swap(core::ptr::null_mut(), o);
    fence(Ordering::SeqCst);
    compiler_fence(Ordering::SeqCst);
    let arc = Arc::new(0u8);
    let _ = Arc::downgrade(&arc).upgrade();
};

#[cfg(test)]
mod tests;
