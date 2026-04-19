use super::{DeviceWatcher, InventoryEventSubscription, WatchRuntime};
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;
use tokio::task::JoinError;

mod refresh;
mod subscription;
mod sync;

pub type AsyncWatchResult<T> = Result<T, AsyncWatchError>;

#[derive(Debug, Error)]
pub enum AsyncWatchError {
    #[error(transparent)]
    Watch(#[from] super::WatchError),
    #[error("tokio blocking task failed: {0}")]
    Join(#[from] JoinError),
}

pub struct AsyncWatchRuntime {
    pub(crate) inner: Arc<RwLock<WatchRuntime>>,
}

pub struct AsyncDeviceWatcher<W> {
    pub(crate) inner: Arc<Mutex<W>>,
}

pub struct AsyncInventoryEventSubscription {
    pub(crate) runtime: Arc<RwLock<WatchRuntime>>,
    pub(crate) subscription: Arc<Mutex<InventoryEventSubscription>>,
}

impl AsyncWatchRuntime {
    pub fn new(runtime: WatchRuntime) -> Self {
        Self {
            inner: Arc::new(RwLock::new(runtime)),
        }
    }

    pub fn from_shared(inner: Arc<RwLock<WatchRuntime>>) -> Self {
        Self { inner }
    }

    pub fn shared(&self) -> Arc<RwLock<WatchRuntime>> {
        Arc::clone(&self.inner)
    }

    pub fn subscribe(&self) -> AsyncInventoryEventSubscription {
        let subscription = {
            let runtime = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            runtime.subscribe_blocking()
        };
        AsyncInventoryEventSubscription {
            runtime: Arc::clone(&self.inner),
            subscription: Arc::new(Mutex::new(subscription)),
        }
    }

    pub fn subscribe_from_start(&self) -> AsyncInventoryEventSubscription {
        let subscription = {
            let runtime = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            runtime.subscribe_from_start_blocking()
        };
        AsyncInventoryEventSubscription {
            runtime: Arc::clone(&self.inner),
            subscription: Arc::new(Mutex::new(subscription)),
        }
    }
}

impl<W> AsyncDeviceWatcher<W>
where
    W: DeviceWatcher + Send + 'static,
{
    pub fn new(watcher: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(watcher)),
        }
    }

    pub fn inner(&self) -> Arc<Mutex<W>> {
        Arc::clone(&self.inner)
    }
}

impl Clone for AsyncWatchRuntime {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Clone for AsyncInventoryEventSubscription {
    fn clone(&self) -> Self {
        Self {
            runtime: Arc::clone(&self.runtime),
            subscription: Arc::clone(&self.subscription),
        }
    }
}
