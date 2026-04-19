use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::watch::{InventoryEventSubscription, WatchRuntime};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn read_lock<T>(lockable: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lockable
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn write_lock<T>(lockable: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lockable
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn with_subscription_runtime_read<T>(
    runtime: &RwLock<WatchRuntime>,
    subscription: &Mutex<InventoryEventSubscription>,
    f: impl FnOnce(&mut InventoryEventSubscription, &WatchRuntime) -> T,
) -> T {
    let mut subscription = lock(subscription);
    let runtime = read_lock(runtime);
    f(&mut subscription, &runtime)
}

pub(crate) fn subscription_has_pending(
    runtime: &RwLock<WatchRuntime>,
    subscription: &Mutex<InventoryEventSubscription>,
) -> bool {
    with_subscription_runtime_read(runtime, subscription, |subscription, runtime| {
        subscription.has_pending(runtime)
    })
}
