//! The atomics and the shared pool handle, from one place.
//!
//! By default these are `core`'s atomics and `alloc`'s `Arc`: nothing else is linked. With the
//! `critical-section` feature, on a target with no compare-and-swap (`riscv32imc`, `thumbv6m`:
//! rustc leaves `target_has_atomic` unset there), they come from `portable-atomic` and
//! `portable-atomic-util`: each read-modify-write (a lease's count, a slot's claim, a counter) is
//! done inside the image's critical section, which the final image supplies
//! (`critical_section::set_impl!`). Loads and stores stay plain. On a target with compare-and-swap
//! the feature changes nothing. Same counts, same orderings, same zero-copy rules on both paths;
//! the counts change per lease, per acquire and per copy, never per row or per pixel.
//!
//! `--cfg styx_portable_atomic` puts a host on the portable path, so its tests run there.

pub(crate) use core::sync::atomic::Ordering;

#[cfg(all(not(target_has_atomic = "ptr"), not(feature = "critical-section")))]
compile_error!(
    "styx-frame: this target has no compare-and-swap; build with the `critical-section` feature \
     (and supply the image's critical section)"
);

#[cfg(not(all(
    feature = "critical-section",
    any(not(target_has_atomic = "ptr"), styx_portable_atomic)
)))]
mod imp {
    pub(crate) use alloc::sync::Arc;
    pub(crate) use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    /// `f` applied to the value until it holds; `Err` with the value when `f` declines.
    pub(crate) fn try_update(
        atomic: &AtomicUsize,
        f: impl FnMut(usize) -> Option<usize>,
    ) -> Result<usize, usize> {
        atomic.try_update(Ordering::Relaxed, Ordering::Relaxed, f)
    }
}

#[cfg(all(
    feature = "critical-section",
    any(not(target_has_atomic = "ptr"), styx_portable_atomic)
))]
mod imp {
    pub(crate) use core::sync::atomic::Ordering;
    pub(crate) use portable_atomic::{AtomicBool, AtomicU32, AtomicUsize};
    pub(crate) use portable_atomic_util::Arc;

    /// `f` applied to the value until it holds; `Err` with the value when `f` declines.
    pub(crate) fn try_update(
        atomic: &AtomicUsize,
        f: impl FnMut(usize) -> Option<usize>,
    ) -> Result<usize, usize> {
        atomic.fetch_update(Ordering::Relaxed, Ordering::Relaxed, f)
    }
}

pub(crate) use imp::{Arc, AtomicBool, AtomicU32, AtomicUsize, try_update};
