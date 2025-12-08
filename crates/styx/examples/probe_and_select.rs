use std::time::Duration;

use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !(cfg!(feature = "v4l2") || cfg!(feature = "libcamera")) {
        println!("Enable the `v4l2` or `libcamera` feature to run this example.");
        return Ok(());
    }
    let devices = probe_all();
    if devices.is_empty() {
        println!("no devices found; enable v4l2/libcamera features and re-run");
        return Ok(());
    }

    println!("found {} device(s):", devices.len());
    for (idx, dev) in devices.iter().enumerate() {
        println!(
            "- [{idx}] {} keys={:?}",
            dev.identity.display, dev.identity.keys
        );
        for backend in &dev.backends {
            println!(
                "  backend={:?} modes={} controls={}",
                backend.kind,
                backend.descriptor.modes.len(),
                backend.descriptor.controls.len()
            );
        }
    }

    let device = &devices[0];
    let backend = device.backends[0].kind;
    let mode = device.backends[0]
        .descriptor
        .modes
        .first()
        .ok_or("device missing modes")?
        .id
        .clone();

    let handle = CaptureRequest::new(device)
        .backend(backend)
        .mode(mode)
        .start()?;

    println!(
        "streaming on {:?} mode {:?}",
        handle.backend(),
        handle.mode().id
    );
    println!("controls:");
    for ctrl in &device.backends[0].descriptor.controls {
        println!("  - {} ({:?})", ctrl.name, ctrl.id);
    }

    let mut frames = 0;
    while frames < 30 {
        match handle.recv_blocking(Duration::from_millis(10)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                println!(
                    "#{frames:03} ts={} fmt={:?} stride={}",
                    meta.timestamp,
                    meta.format.code,
                    frame.plane_strides().first().copied().unwrap_or_default()
                );
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    handle.stop();
    Ok(())
}
