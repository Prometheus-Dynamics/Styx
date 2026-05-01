#[cfg(feature = "simulation-bevy")]
use std::env;
#[cfg(feature = "simulation-bevy")]
use std::path::PathBuf;
#[cfg(feature = "simulation-bevy")]
use std::time::Duration;

#[cfg(feature = "simulation-bevy")]
use styx::capture_api::{SimulationDeviceConfig, SimulationOutputMode, make_simulation_device};
#[cfg(feature = "preview-window")]
use styx::extras::preview_window::PreviewWindow;
#[cfg(feature = "simulation-bevy")]
use styx::prelude::*;

#[cfg(not(feature = "simulation-bevy"))]
fn main() {
    eprintln!("Enable the `simulation-bevy` feature to run this example.");
}

#[cfg(feature = "simulation-bevy")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scene_path = env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/simulation/BoxTextured.glb")
    });

    let output_mode = match env::var("STYX_SIM_OUTPUT").ok().as_deref() {
        Some("depth") => SimulationOutputMode::Depth,
        Some("normals") => SimulationOutputMode::Normals,
        Some("segmentation") => SimulationOutputMode::Segmentation,
        _ => SimulationOutputMode::Rgb,
    };

    let config = SimulationDeviceConfig {
        output_mode,
        ..SimulationDeviceConfig::default()
    };
    let device = make_simulation_device("simulation", scene_path, config);
    let backend = &device.backends[0];
    let handle = CaptureRequest::new(&device).start()?;

    println!("available controls:");
    for ctrl in &backend.descriptor.controls {
        println!(
            "- {} ({:?}) access={:?} default={:?}",
            ctrl.name, ctrl.id, ctrl.access, ctrl.default
        );
    }

    #[cfg(feature = "preview-window")]
    let mut preview = PreviewWindow::for_descriptor("simulation", &backend.descriptor).ok();

    let mut frames = 0;
    while frames < 120 {
        match handle.recv_blocking(Duration::from_millis(16)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                let meta = frame.meta();
                println!(
                    "#{frames:03} ts={} fmt={:?} stride={}",
                    meta.timestamp,
                    meta.format.code,
                    frame.plane_strides().first().copied().unwrap_or_default()
                );
                #[cfg(feature = "preview-window")]
                if let Some(win) = preview.as_mut() {
                    let _ = win.show_if_open(&frame);
                }
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    handle.stop();
    Ok(())
}
