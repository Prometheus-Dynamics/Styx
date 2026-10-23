//! The same glTF scene through both simulation renderers: Bevy's (GPU) and the simulated
//! cameras' (`scene=gltf`, CPU). The scene's square must land where the glTF camera's pinhole
//! puts it in both, with its texture the right way round. Needs a GPU: run with
//! `--run-ignored all`.

use std::path::PathBuf;
use std::time::Duration;

use styx::prelude::*;
use styx::simulation::{SimulationDeviceConfig, make_simulation_device};

const W: u32 = 640;
const H: u32 = 400;
/// The fixture's camera: vertical field of view 0.9 rad.
const YFOV: f64 = 0.9;

/// Where the fixture's pinhole puts the point (x, y, z) of the camera's frame.
fn pixel(x: f64, y: f64, z: f64) -> (f64, f64) {
    let f = f64::from(H) / 2.0 / (YFOV / 2.0).tan();
    (
        (f64::from(W) - 1.0) / 2.0 + f * x / -z,
        (f64::from(H) - 1.0) / 2.0 - f * y / -z,
    )
}

/// The frame's luma (the mean of R, G, B).
fn luma(frame: &FrameLease) -> Vec<f32> {
    let planes = frame.planes();
    let data = planes[0].data();
    let stride = frame.layout_slice()[0].stride;
    (0..H as usize)
        .flat_map(|y| {
            (0..W as usize).map(move |x| {
                let p = &data[y * stride + x * 3..y * stride + x * 3 + 3];
                (f32::from(p[0]) + f32::from(p[1]) + f32::from(p[2])) / 3.0
            })
        })
        .collect()
}

/// The first column from the left on row `y` darker than `level`.
fn first_below(img: &[f32], y: usize, level: f32) -> Option<usize> {
    (0..W as usize).find(|&x| img[y * W as usize + x] < level)
}

#[test]
#[ignore = "needs a GPU"]
fn bevy_renders_the_shared_gltf_scene_where_the_lens_puts_it() {
    let scene = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/sim_scene.gltf");
    let mut config = SimulationDeviceConfig::default();
    config.sensor.width = W;
    config.sensor.height = H;
    // Bevy's field of view is 2 atan(sensor height / 2 focal length).
    config.sensor.sensor_height_mm = (2.0 * 35.0 * (YFOV / 2.0).tan()) as f32;
    config.sensor.sensor_width_mm = config.sensor.sensor_height_mm * W as f32 / H as f32;
    config.lens.focal_length_mm = 35.0;
    config.pose.translation_m = [0.0, 0.0, 0.0];
    config.clear_color_rgba = [0.0, 0.0, 0.0, 1.0];
    let device = make_simulation_device("cross-check", scene, config);
    let handle = CaptureRequest::new(&device).start().unwrap();
    // Frames until the scene has loaded (the backdrop's grey fills the frame's corner).
    let mut img = None;
    for _ in 0..600 {
        if let RecvOutcome::Data(f) = handle.recv_blocking(Duration::from_millis(50)) {
            let l = luma(&f);
            if l[5 * W as usize + 5] > 10.0 {
                img = Some(l);
                break;
            }
        }
    }
    let img = img.expect("the scene rendered");
    if let Ok(dir) = std::env::var("STYX_CROSS_DUMP") {
        let mut pgm = format!("P5\n{W} {H}\n255\n").into_bytes();
        pgm.extend(img.iter().map(|&v| v.round() as u8));
        std::fs::write(format!("{dir}/bevy.pgm"), pgm).unwrap();
    }
    let at = |(x, y): (f64, f64)| img[y.round() as usize * W as usize + x.round() as usize];
    // Unlit: the top left cell white, the one right of it black, the backdrop 0.2 (sRGB
    // encoded: about 124) beside the square.
    let white = at(pixel(-0.4375, 0.4375, -3.0));
    let black = at(pixel(-0.3125, 0.4375, -3.0));
    let backdrop = at(pixel(-1.5, 0.0, -5.0));
    assert!(white > 240.0, "white {white}");
    assert!(black < 15.0, "black {black}");
    assert!((110.0..140.0).contains(&backdrop), "backdrop {backdrop}");
    // The square's left edge on the row of its fourth cell row (a black cell at the edge):
    // within a pixel of the pinhole's.
    let (edge, y) = pixel(-0.5, 0.0625, -3.0);
    let x = first_below(&img, y.round() as usize, backdrop / 2.0).expect("an edge") as f64;
    // The first dark pixel's centre is at most half a pixel inside the edge.
    assert!(
        (x - edge).abs() <= 1.0,
        "edge at {x}, the lens says {edge:.2}"
    );
}
