use std::thread;
use std::time::Duration;

use styx::prelude::*;

fn print_devices(devices: &[ProbedDevice]) {
    println!("inventory: {} device(s)", devices.len());
    for device in devices {
        println!(
            "- {} keys={:?} backends={}",
            device.identity.display,
            device.identity.keys,
            device.backends.len()
        );
        for backend in &device.backends {
            println!(
                "  backend={:?} modes={} controls={}",
                backend.kind,
                backend.descriptor.modes.len(),
                backend.descriptor.controls.len()
            );
        }
    }
}

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
            InventoryEvent::Added(device) => {
                println!("added {}", device.identity.display);
            }
            InventoryEvent::Removed(device) => {
                println!("removed {}", device.identity.display);
            }
            InventoryEvent::Changed(changed) => {
                println!(
                    "changed {} -> {}",
                    changed.before.identity.display, changed.after.identity.display
                );
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(feature = "hotplug") {
        println!("Enable the `hotplug` feature to run this example.");
        return Ok(());
    }
    if !(cfg!(feature = "v4l2") || cfg!(feature = "libcamera")) {
        println!("Enable the `v4l2` or `libcamera` feature to run this example.");
        return Ok(());
    }

    let mut watcher = CompositeWatcher::new();
    #[cfg(all(feature = "hotplug", target_os = "linux"))]
    watcher.push(LinuxVideoFsWatcher::new()?);
    #[cfg(all(feature = "hotplug", feature = "libcamera"))]
    {
        if let Ok(libcamera) = LibcameraHotplugWatcher::new() {
            watcher.push(libcamera);
        }
    }

    if watcher.is_empty() {
        println!("No watcher backends available in this build.");
        return Ok(());
    }

    let mut runtime = WatchRuntime::new();
    let initial = runtime.refresh_uncached();
    print_devices(&initial.probe_result.devices);
    println!("watching for inventory changes; press Ctrl+C to stop");

    loop {
        if let Some(report) = runtime.poll_watcher_and_refresh(&mut watcher)? {
            print_report(&report);
        }
        thread::sleep(Duration::from_millis(250));
    }
}
