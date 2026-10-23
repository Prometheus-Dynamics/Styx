use super::*;

fn fixture(name: &str) -> String {
    format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn scene(name: &str, extra: &[(&str, &str)]) -> GltfScene {
    let mut params = vec![("path".to_string(), fixture(name))];
    params.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    GltfScene::from_params(&params).unwrap()
}

fn ctx(time: f64) -> FrameContext {
    FrameContext {
        index: 0,
        time,
        exposure: 0.0,
        width: 640,
        height: 400,
        field: [0.0, 0.0, 1.0, 1.0],
    }
}

/// The pinhole of the fixture's camera (0.9 rad vertically) over 640x400.
fn pixel(x: f64, y: f64, z: f64) -> [f64; 2] {
    let f = 200.0 / 0.45f64.tan();
    [319.5 + f * x / -z, 199.5 - f * y / -z]
}

/// Within glTF's single precision.
fn near(a: [f64; 2], b: [f64; 2]) -> bool {
    (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
}

/// The square's corners where the camera's pinhole puts them; the point behind the square is
/// hidden; the camera at its node's pose.
#[test]
fn truth_points_follow_the_lens_and_hide_behind_the_scene() {
    let s = scene("sim_scene.gltf", &[]);
    let (_, truth) = s.frame_at(&ctx(0.0));
    let truth = truth.unwrap();
    let get = |n: &str| truth.points.iter().find(|p| p.0 == n).map(|p| p.1);
    assert!(near(get("square/tl").unwrap(), pixel(-0.5, 0.5, -3.0)));
    assert!(near(get("square/br").unwrap(), pixel(0.5, -0.5, -3.0)));
    assert_eq!(get("hidden"), None);
    assert_eq!(truth.points.len(), 4);
    assert_eq!(truth.camera.0, [0.0, 0.0, 0.0]);
}

/// The square's checker as its texture says (nearest sampling: white top left), the backdrop's
/// grey around it.
#[test]
fn surfaces_show_their_material() {
    let s = scene("sim_scene.gltf", &[("antialias", "off")]);
    let (frame, _) = s.frame_at(&ctx(0.0));
    let SceneFrame::Rgb { light, .. } = &*frame else {
        panic!("rgb");
    };
    let at = |[x, y]: [f64; 2]| light[y.round() as usize * 640 + x.round() as usize][0];
    // The centre of the top left cell (white), of the cell right of it (black).
    assert!((at(pixel(-0.4375, 0.4375, -3.0)) - LIGHT as f32).abs() < 1e-3);
    assert!(at(pixel(-0.3125, 0.4375, -3.0)) < 1e-3);
    // The backdrop (0.2) beside the square.
    assert!((at(pixel(-1.5, 0.0, -5.0)) - 0.2 * LIGHT as f32).abs() < 1e-3);
}

/// An animated camera moves along its track, looped.
#[test]
fn the_camera_follows_its_animation() {
    let s = scene("sim_scene_moving.gltf", &[]);
    assert!(s.moves());
    for (t, x) in [(0.0, 0.0), (1.0, 0.5), (2.0, 1.0), (3.0, 0.5), (5.0, 0.5)] {
        let (_, truth) = s.frame_at(&ctx(t));
        let p = truth.unwrap().camera.0;
        assert!((p[0] - x).abs() < 1e-6, "{t} s: {p:?}");
    }
}

/// The lens from the spec's keys (a fisheye) instead of the glTF camera's.
#[test]
fn the_specs_lens_is_used() {
    let s = scene(
        "sim_scene.gltf",
        &[
            ("lens", "fisheye"),
            ("lens_width", "640"),
            ("lens_height", "400"),
            ("fx", "300"),
            ("cx", "319.5"),
            ("cy", "199.5"),
            ("k1", "0.05"),
        ],
    );
    let (_, truth) = s.frame_at(&ctx(0.0));
    let truth = truth.unwrap();
    let tl = truth.points.iter().find(|p| p.0 == "square/tl").unwrap().1;
    // Equidistant: r = f θ (1 + k1 θ²) along the corner's direction.
    let r = (0.5f64 * 0.5 + 0.5 * 0.5).sqrt();
    let theta = (r / 3.0).atan();
    let rd = 300.0 * theta * (1.0 + 0.05 * theta * theta);
    let expected = [319.5 - rd * (0.5 / r), 199.5 - rd * (0.5 / r)];
    assert!(near(tl, expected), "{tl:?} vs {expected:?}");
    assert!(
        GltfScene::from_params(&[
            ("path".into(), fixture("sim_scene.gltf")),
            ("bogus".into(), "1".into())
        ])
        .is_err()
    );
}
