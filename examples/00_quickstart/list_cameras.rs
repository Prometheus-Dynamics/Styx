//! Lists every camera Styx can open, through every enabled backend: USB and other V4L2
//! cameras (`v4l2`), sensors Styx drives itself through its sensor bridge or their kernel
//! driver (`native`), and libcamera cameras (`libcamera`). For each: its modes (pixel format,
//! size, frame rates: the listed rates, and the exact range for sensors whose timing Styx
//! controls), its controls, and backend properties such as the ISP a native camera uses.
//!
//! ```sh
//! cargo run -p styx-examples --features native,v4l2 --bin list_cameras
//! cargo run -p styx-examples --features native,v4l2,libcamera --bin list_cameras   # + libcamera
//! ```
//!
//! A camera reachable through several backends is one device with several backends (the
//! planner picks between them). Probe errors (a node that is busy, a sensor without a
//! description) are printed too.

use styx::prelude::*;

fn fps(i: Interval) -> String {
    format!("{:.3}", i.fps())
}

fn rates(mode: &Mode) -> String {
    let mut listed: Vec<String> = mode.intervals.iter().map(|i| fps(*i)).collect();
    listed.dedup();
    let mut out = if listed.is_empty() {
        String::new()
    } else {
        format!("{} fps", listed.join(", "))
    };
    if let Some(s) = mode.interval_stepwise {
        // `min` is the shortest interval: the fastest rate.
        out.push_str(&format!(" (any rate {}..{} fps)", fps(s.max), fps(s.min)));
    }
    out
}

fn value(v: &ControlValue) -> String {
    match v {
        ControlValue::None => "-".into(),
        ControlValue::Bool(b) => b.to_string(),
        ControlValue::Int(i) => i.to_string(),
        ControlValue::Uint(u) => u.to_string(),
        ControlValue::Float(f) => format!("{f}"),
        other => format!("{other:?}"),
    }
}

fn main() {
    if !(cfg!(feature = "v4l2") || cfg!(feature = "native") || cfg!(feature = "libcamera")) {
        println!("Enable the `v4l2`, `native` or `libcamera` feature to list cameras.");
        return;
    }
    let probe = styx::probe_all_with_errors();
    for e in &probe.errors {
        println!("probe error: {e}");
    }
    if probe.devices.is_empty() {
        println!("no cameras");
    }
    for device in &probe.devices {
        println!("{}", device.identity.display);
        println!("  keys: {}", device.identity.keys.join(", "));
        for backend in &device.backends {
            let props: Vec<String> = backend
                .properties
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            println!("  backend {}: {}", backend.kind, props.join(" "));
            for mode in &backend.descriptor.modes {
                let res = mode.format.resolution;
                println!(
                    "    {} {:>4}x{:<4} {}",
                    mode.format.code,
                    res.width,
                    res.height,
                    rates(mode)
                );
            }
            for c in &backend.descriptor.controls {
                println!(
                    "    control {:<28} {}..{} (default {})",
                    c.name,
                    value(&c.min),
                    value(&c.max),
                    value(&c.default)
                );
            }
        }
    }
}
