#[cfg(all(feature = "simulation-bevy", feature = "preview-window"))]
use std::path::PathBuf;
#[cfg(all(feature = "simulation-bevy", feature = "preview-window"))]
use std::time::Duration;

#[cfg(feature = "preview-window")]
use styx::extras::preview_window::PreviewWindow;
#[cfg(all(feature = "simulation-bevy", feature = "preview-window"))]
use styx::prelude::*;

#[cfg(not(all(feature = "simulation-bevy", feature = "preview-window")))]
fn main() {
    eprintln!("Enable the `simulation-bevy` and `preview-window` features to run this example.");
}

#[cfg(all(feature = "simulation-bevy", feature = "preview-window"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scene_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/simulation/BoxTextured.glb")
        });

    let device =
        make_simulation_device("simulation", scene_path, SimulationDeviceConfig::default());
    let backend = &device.backends[0];
    let output_mode_ctrl = backend
        .descriptor
        .controls
        .iter()
        .find(|ctrl| ctrl.name == "simulation.output.mode")
        .ok_or("missing simulation.output.mode control")?
        .id;

    let handle = CaptureRequest::new(&device).start()?;
    let mut preview = PreviewWindow::for_descriptor("simulation-cycle", &backend.descriptor)
        .map_err(std::io::Error::other)?;

    let cycle = [
        ("rgb", 0u32),
        ("depth", 1u32),
        ("normals", 2u32),
        ("segmentation", 3u32),
    ];
    let mut cycle_index = 0usize;
    handle.set_control(output_mode_ctrl, ControlValue::Uint(cycle[cycle_index].1))?;
    println!("mode -> {}", cycle[cycle_index].0);

    let mut frames = 0usize;
    while preview.is_open() && frames < 480 {
        match handle.recv_blocking(Duration::from_millis(33)) {
            RecvOutcome::Data(frame) => {
                frames += 1;
                if frames.is_multiple_of(30) {
                    cycle_index = (cycle_index + 1) % cycle.len();
                    handle
                        .set_control(output_mode_ctrl, ControlValue::Uint(cycle[cycle_index].1))?;
                    println!("mode -> {}", cycle[cycle_index].0);
                }
                if !preview.show_if_open(&frame) {
                    break;
                }
            }
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        }
    }

    handle.stop();
    Ok(())
}
