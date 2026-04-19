use super::sync::{lock, subscription_has_pending, with_subscription_runtime_read};
use super::{AsyncInventoryEventSubscription, AsyncWatchResult};
use crate::watch::{InventoryEvent, InventoryEventCursor};
use std::time::Duration;

impl AsyncInventoryEventSubscription {
    pub fn cursor(&self) -> InventoryEventCursor {
        lock(&self.subscription).cursor()
    }

    pub async fn cursor_async(&self) -> AsyncWatchResult<InventoryEventCursor> {
        let subscription = std::sync::Arc::clone(&self.subscription);
        Ok(tokio::task::spawn_blocking(move || lock(&subscription).cursor()).await?)
    }

    pub async fn has_pending_async(&self) -> AsyncWatchResult<bool> {
        let runtime = std::sync::Arc::clone(&self.runtime);
        let subscription = std::sync::Arc::clone(&self.subscription);
        Ok(tokio::task::spawn_blocking(move || {
            with_subscription_runtime_read(&runtime, &subscription, |subscription, runtime| {
                subscription.has_pending(runtime)
            })
        })
        .await?)
    }

    pub async fn poll(&self) -> AsyncWatchResult<Vec<InventoryEvent>> {
        let runtime = std::sync::Arc::clone(&self.runtime);
        let subscription = std::sync::Arc::clone(&self.subscription);
        Ok(tokio::task::spawn_blocking(move || {
            with_subscription_runtime_read(&runtime, &subscription, |subscription, runtime| {
                subscription.poll(runtime).to_vec()
            })
        })
        .await?)
    }

    pub async fn wait_for_update(&self, timeout: Option<Duration>) -> AsyncWatchResult<bool> {
        let subscription = std::sync::Arc::clone(&self.subscription);
        Ok(tokio::task::spawn_blocking(move || {
            let subscription = lock(&subscription);
            subscription.wait_for_update(timeout)
        })
        .await?)
    }

    pub async fn wait_and_poll_next(
        &self,
        timeout: Option<Duration>,
    ) -> AsyncWatchResult<Option<Vec<InventoryEvent>>> {
        let runtime = std::sync::Arc::clone(&self.runtime);
        let subscription = std::sync::Arc::clone(&self.subscription);
        Ok(tokio::task::spawn_blocking(move || {
            if subscription_has_pending(&runtime, &subscription) {
                return Some(with_subscription_runtime_read(
                    &runtime,
                    &subscription,
                    |subscription, runtime| subscription.poll(runtime).to_vec(),
                ));
            }

            let updated = {
                let subscription = lock(&subscription);
                subscription.wait_for_update(timeout)
            };
            if !updated {
                return None;
            }

            Some(with_subscription_runtime_read(
                &runtime,
                &subscription,
                |subscription, runtime| subscription.poll(runtime).to_vec(),
            ))
        })
        .await?)
    }
}
