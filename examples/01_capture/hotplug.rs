//! Hotplug: cameras coming and going while the program runs. A watcher (kernel uevents for
//! video, media and USB devices, plus libcamera's hotplug signal when that backend is built)
//! wakes the inventory, which re-probes and reports what was added, removed or changed. Here
//! every camera that appears gets a capture, and a camera that goes away has its capture
//! dropped.
//!
//! ```sh
//! cargo run -p styx-examples --features hotplug,native,v4l2 --bin hotplug -- [seconds]
//! # meanwhile: unplug / replug a USB camera, or
//! #   echo 0 > /sys/bus/usb/devices/3-1/authorized; sleep 3; echo 1 > /sys/bus/usb/devices/3-1/authorized
//! ```

use std::collections::HashMap;
use std::time::{Duration, Instant};

use styx::prelude::*;

/// A capture per camera, keyed by the camera's display name.
type Captures = HashMap<String, (CaptureHandle, u64)>;

fn start(device: &ProbedDevice, captures: &mut Captures) {
    match CaptureRequest::new(device).start() {
        Ok(handle) => {
            println!("  capturing from {}", device.identity.display);
            captures.insert(device.identity.display.clone(), (handle, 0));
        }
        Err(e) => println!("  {}: {e}", device.identity.display),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(feature = "hotplug") {
        println!("Enable the `hotplug` feature to run this example.");
        return Ok(());
    }
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let mut watcher = CompositeWatcher::new();
    #[cfg(all(feature = "hotplug", target_os = "linux"))]
    watcher.push(LinuxVideoFsWatcher::new()?);
    #[cfg(all(feature = "hotplug", feature = "libcamera"))]
    if let Ok(libcamera) = LibcameraHotplugWatcher::new() {
        watcher.push(libcamera);
    }

    let mut inventory = WatchRuntime::new();
    let mut captures = Captures::new();
    for device in &inventory.refresh_uncached().probe_result.devices {
        start(device, &mut captures);
    }
    println!("watching for {seconds} s");
    let started = Instant::now();
    let mut reported = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        if let Some(report) = inventory.poll_watcher_and_refresh(&mut watcher)? {
            for event in report.diff.events() {
                let at = started.elapsed().as_secs_f64();
                match event {
                    InventoryEvent::Added(device) => {
                        println!("{at:6.2} s added {}", device.identity.display);
                        start(&device, &mut captures);
                    }
                    InventoryEvent::Removed(device) => {
                        println!("{at:6.2} s removed {}", device.identity.display);
                        if let Some((handle, frames)) = captures.remove(&device.identity.display) {
                            println!("  its capture ended after {frames} frames");
                            handle.stop();
                        }
                    }
                    InventoryEvent::Changed(c) => {
                        println!("{at:6.2} s changed {}", c.after.identity.display)
                    }
                }
            }
        }
        // Drain frames; a capture whose camera vanished reports `Closed`.
        for (handle, frames) in captures.values_mut() {
            while let RecvOutcome::Data(_) = handle.recv() {
                *frames += 1;
            }
        }
        if reported.elapsed() > Duration::from_secs(5) {
            reported = Instant::now();
            let counts: Vec<String> = captures
                .iter()
                .map(|(name, (_, n))| format!("{name}: {n}"))
                .collect();
            println!("frames so far: {}", counts.join(", "));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for (_, (handle, _)) in captures {
        handle.stop();
    }
    Ok(())
}
