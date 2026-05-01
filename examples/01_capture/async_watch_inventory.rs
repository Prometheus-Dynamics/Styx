#[cfg(feature = "async")]
use std::time::Duration;

#[cfg(feature = "async")]
use styx::prelude::*;
#[cfg(feature = "async")]
use styx::watch::{AsyncDeviceWatcher, AsyncWatchRuntime};

#[cfg(feature = "async")]
fn print_report(report: &WatchRefreshReport) {
    if report.diff.is_empty() {
        return;
    }
    for event in &report.watch_events {
        println!(
            "watcher={} backends={:?} paths={:?}",
            event.watcher, event.backends, event.paths
        );
    }
    for event in report.diff.events() {
        match event {
            InventoryEvent::Added(device) => println!("added {}", device.identity.display),
            InventoryEvent::Removed(device) => println!("removed {}", device.identity.display),
            InventoryEvent::Changed(changed) => println!(
                "changed {} -> {}",
                changed.before.identity.display, changed.after.identity.display
            ),
        }
    }
}

#[cfg(feature = "async")]
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(feature = "hotplug") {
        println!("Enable the `hotplug` feature to run this example.");
        return Ok(());
    }
    if !(cfg!(feature = "v4l2") || cfg!(feature = "libcamera")) {
        println!("Enable the `v4l2` or `libcamera` feature to run this example.");
        return Ok(());
    }

    #[cfg(any(
        all(feature = "hotplug", target_os = "linux"),
        all(feature = "hotplug", feature = "libcamera")
    ))]
    let mut composite = CompositeWatcher::new();
    #[cfg(not(any(
        all(feature = "hotplug", target_os = "linux"),
        all(feature = "hotplug", feature = "libcamera")
    )))]
    let composite = CompositeWatcher::new();
    #[cfg(all(feature = "hotplug", target_os = "linux"))]
    composite.push(LinuxVideoFsWatcher::new()?);
    #[cfg(all(feature = "hotplug", feature = "libcamera"))]
    {
        if let Ok(libcamera) = LibcameraHotplugWatcher::new() {
            composite.push(libcamera);
        }
    }

    if composite.is_empty() {
        println!("No watcher backends available in this build.");
        return Ok(());
    }

    let watcher = AsyncDeviceWatcher::new(composite);
    let runtime = AsyncWatchRuntime::new(WatchRuntime::new());
    let initial = runtime.refresh_uncached().await?;
    println!(
        "inventory: {} device(s)",
        initial.probe_result.devices.len()
    );
    println!("watching for inventory changes; press Ctrl+C to stop");

    loop {
        if let Some(report) = runtime.poll_watcher_and_refresh(&watcher).await? {
            print_report(&report);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(not(feature = "async"))]
fn main() {
    println!("Enable the `async` feature to run this example.");
}
