#[cfg(feature = "async")]
mod async_runtime;
mod config;
mod error;
mod event;
#[cfg(all(feature = "hotplug", feature = "libcamera"))]
mod libcamera;
#[cfg(all(feature = "hotplug", target_os = "linux"))]
mod linux;
mod runtime;
mod subscription;
mod watcher;

#[cfg(feature = "async")]
pub use async_runtime::{
    AsyncDeviceWatcher, AsyncInventoryEventSubscription, AsyncWatchError, AsyncWatchResult,
    AsyncWatchRuntime,
};
pub use config::WatchRuntimeConfig;
pub use error::WatchError;
pub use event::{ChangedDevice, InventoryDiff, InventoryEvent};
#[cfg(all(feature = "hotplug", feature = "libcamera"))]
pub use libcamera::LibcameraHotplugWatcher;
#[cfg(all(feature = "hotplug", target_os = "linux"))]
pub use linux::LinuxVideoFsWatcher;
pub use runtime::{InventoryEventRetentionStats, WatchRefreshReport, WatchRuntime};
pub use subscription::{InventoryEventCursor, InventoryEventPoll, InventoryEventSubscription};
pub use watcher::{CompositeWatcher, DeviceWatchEvent, DeviceWatcher};

#[cfg(test)]
mod tests;
