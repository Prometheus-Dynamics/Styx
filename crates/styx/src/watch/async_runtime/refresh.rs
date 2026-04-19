use super::sync::{lock, read_lock, write_lock};
use super::{AsyncDeviceWatcher, AsyncWatchError, AsyncWatchResult, AsyncWatchRuntime};
use crate::watch::{DeviceWatcher, WatchRefreshReport};

impl AsyncWatchRuntime {
    pub async fn refresh(&self) -> AsyncWatchResult<WatchRefreshReport> {
        let inner = self.shared();
        Ok(tokio::task::spawn_blocking(move || {
            let mut runtime = write_lock(&inner);
            runtime.refresh()
        })
        .await?)
    }

    pub async fn refresh_uncached(&self) -> AsyncWatchResult<WatchRefreshReport> {
        let inner = self.shared();
        Ok(tokio::task::spawn_blocking(move || {
            let mut runtime = write_lock(&inner);
            runtime.refresh_uncached()
        })
        .await?)
    }

    pub async fn poll_watcher_and_refresh<W>(
        &self,
        watcher: &AsyncDeviceWatcher<W>,
    ) -> AsyncWatchResult<Option<WatchRefreshReport>>
    where
        W: DeviceWatcher + Send + 'static,
    {
        let inner = self.shared();
        let watcher_inner = watcher.inner();
        tokio::task::spawn_blocking(move || {
            let mut watcher = lock(&watcher_inner);
            let mut runtime = write_lock(&inner);
            runtime
                .poll_watcher_and_refresh(&mut *watcher)
                .map_err(AsyncWatchError::from)
        })
        .await?
    }

    pub async fn poll_watcher_and_refresh_incremental<W>(
        &self,
        watcher: &AsyncDeviceWatcher<W>,
    ) -> AsyncWatchResult<Option<WatchRefreshReport>>
    where
        W: DeviceWatcher + Send + 'static,
    {
        let inner = self.shared();
        let watcher_inner = watcher.inner();
        tokio::task::spawn_blocking(move || {
            let mut watcher = lock(&watcher_inner);
            let mut runtime = write_lock(&inner);
            runtime
                .poll_watcher_and_refresh_incremental(&mut *watcher)
                .map_err(AsyncWatchError::from)
        })
        .await?
    }

    pub async fn events(&self) -> AsyncWatchResult<Vec<crate::watch::InventoryEvent>> {
        let inner = self.shared();
        Ok(tokio::task::spawn_blocking(move || {
            let runtime = read_lock(&inner);
            runtime.events().to_vec()
        })
        .await?)
    }
}
