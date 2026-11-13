//! Proof tests for the chooser. No hardware: each backend's listing is a fixture.

use std::path::PathBuf;

use styx_select::{
    Anchor, Backend, Config, LibcameraLister, ListError, Listed, NativeLister, Reason, Scope,
    SelectError, Skip, V4l2Lister, plan, select,
};

fn native(key: &str, anchors: Vec<Anchor>) -> Listed {
    Listed {
        key: key.to_string(),
        anchors,
        virtual_node: false,
    }
}

fn lemnos(name: &str) -> Anchor {
    Anchor::Lemnos(name.to_string())
}

fn usb(port: &str) -> Anchor {
    Anchor::Usb {
        port: port.to_string(),
        ids: None,
    }
}

fn sysfs(path: &str) -> Anchor {
    Anchor::Sysfs(PathBuf::from(path))
}

fn node(path: &str, anchors: Vec<Anchor>, virtual_node: bool) -> Listed {
    let mut all = vec![Anchor::VideoNode(PathBuf::from(path))];
    all.extend(anchors);
    Listed {
        key: path.to_string(),
        anchors: all,
        virtual_node,
    }
}

/// libcamera's listing: its own id, plus the sysfs device it was built from (the anchor that
/// lets it meet a V4L2 or native listing of the same device).
fn libcamera(id: &str, sysfs_device: Option<&str>) -> Listed {
    let mut anchors = vec![Anchor::Libcamera(id.to_string())];
    anchors.extend(sysfs_device.map(sysfs));
    Listed {
        key: id.to_string(),
        anchors,
        virtual_node: false,
    }
}

fn owner_of(plan: &styx_select::Plan, id: &str) -> (Backend, Reason) {
    let cam = plan
        .cameras
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no camera {id} in {plan:?}"));
    (cam.owner, cam.reason)
}

#[test]
fn a_native_chain_wins_over_libcamera_and_v4l2() {
    let p = plan(
        vec![native(
            "sensor0",
            vec![lemnos("sensor0"), sysfs("/sys/d/csi0")],
        )],
        vec![libcamera("/base/sensor0", Some("/sys/d/csi0"))],
        vec![node("/dev/video0", vec![sysfs("/sys/d/csi0")], false)],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(p.cameras.len(), 1, "{p:?}");
    let cam = &p.cameras[0];
    assert_eq!(cam.owner, Backend::Native);
    assert_eq!(cam.reason, Reason::NativeChain);
    assert_eq!(cam.key, "sensor0");
    assert_eq!(cam.id, "lemnos:sensor0", "the Lemnos name is the stable id");
    assert_eq!(
        cam.seen_by,
        [Backend::Native, Backend::Libcamera, Backend::V4l2]
    );
}

#[test]
fn libcamera_listed_camera_without_a_native_chain() {
    let p = plan(
        vec![],
        vec![libcamera("/base/imx", None)],
        vec![node("/dev/video4", vec![], false)],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(p.cameras.len(), 2, "no shared anchor: two cameras, {p:?}");
    assert_eq!(
        owner_of(&p, "libcamera:/base/imx"),
        (Backend::Libcamera, Reason::LibcameraListed)
    );
    assert_eq!(
        owner_of(&p, "video:/dev/video4"),
        (Backend::V4l2, Reason::V4l2Listed)
    );
}

#[test]
fn v4l2_only_camera_is_v4l2() {
    let p = plan(
        vec![],
        vec![],
        vec![node("/dev/video2", vec![usb("0000:00:14.0-2")], false)],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(p.cameras.len(), 1);
    assert_eq!(p.cameras[0].owner, Backend::V4l2);
    assert_eq!(p.cameras[0].reason, Reason::V4l2Listed);
    assert_eq!(
        p.cameras[0].id, "usb:0000:00:14.0-2",
        "a USB port outranks the node path"
    );
}

#[test]
fn forced_global_choice_wins_over_the_default() {
    let config = Config {
        force: Some(Backend::Libcamera),
        ..Config::default()
    };
    let p = plan(
        vec![native(
            "sensor0",
            vec![lemnos("sensor0"), sysfs("/sys/d/csi0")],
        )],
        vec![libcamera("/base/sensor0", Some("/sys/d/csi0"))],
        vec![],
        &config,
    )
    .unwrap();
    let cam = &p.cameras[0];
    assert_eq!(cam.owner, Backend::Libcamera);
    assert_eq!(
        cam.reason,
        Reason::Forced {
            backend: Backend::Libcamera,
            scope: Scope::Global
        }
    );
    assert_eq!(cam.key, "/base/sensor0");
}

#[test]
fn forced_per_camera_choice_wins_over_the_global_one() {
    let config = Config {
        force: Some(Backend::V4l2),
        per_camera: vec![("lemnos:sensor0".to_string(), Backend::Native)],
        ..Config::default()
    };
    let p = plan(
        vec![native(
            "sensor0",
            vec![lemnos("sensor0"), sysfs("/sys/d/csi0")],
        )],
        vec![],
        vec![node("/dev/video0", vec![sysfs("/sys/d/csi0")], false)],
        &config,
    )
    .unwrap();
    assert_eq!(p.cameras.len(), 1);
    assert_eq!(p.cameras[0].owner, Backend::Native);
    assert_eq!(
        p.cameras[0].reason,
        Reason::Forced {
            backend: Backend::Native,
            scope: Scope::Camera
        }
    );
}

#[test]
fn forcing_a_backend_that_does_not_list_the_camera_is_an_error_naming_both() {
    let config = Config {
        per_camera: vec![("libcamera:/base/imx".to_string(), Backend::Native)],
        ..Config::default()
    };
    let err = plan(vec![], vec![libcamera("/base/imx", None)], vec![], &config).unwrap_err();
    match &err {
        SelectError::ForcedMissing {
            camera,
            backend,
            seen_by,
        } => {
            assert_eq!(camera, "libcamera:/base/imx");
            assert_eq!(*backend, Backend::Native);
            assert_eq!(seen_by, &[Backend::Libcamera]);
        }
        other => panic!("expected ForcedMissing, got {other:?}"),
    }
    let text = err.to_string();
    assert!(
        text.contains("libcamera:/base/imx") && text.contains("native"),
        "{text}"
    );
}

#[test]
fn a_global_force_fails_for_a_camera_the_backend_does_not_see() {
    let config = Config {
        force: Some(Backend::Native),
        ..Config::default()
    };
    let err = plan(
        vec![],
        vec![],
        vec![node("/dev/video2", vec![], false)],
        &config,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            SelectError::ForcedMissing {
                backend: Backend::Native,
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn config_naming_a_camera_nobody_lists_is_an_error() {
    let config = Config {
        per_camera: vec![("lemnos:ghost".to_string(), Backend::Native)],
        ..Config::default()
    };
    let err = plan(vec![], vec![], vec![], &config).unwrap_err();
    assert_eq!(
        err,
        SelectError::UnknownCamera {
            camera: "lemnos:ghost".to_string()
        }
    );
}

#[test]
fn a_camera_seen_by_libcamera_and_v4l2_appears_once() {
    // Both backends see the same sysfs device: one camera, owned by libcamera, seen by both.
    let p = plan(
        vec![],
        vec![libcamera("/base/imx", Some("/sys/d/imx"))],
        vec![node("/dev/video0", vec![sysfs("/sys/d/imx")], false)],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(p.cameras.len(), 1, "{p:?}");
    let cam = &p.cameras[0];
    assert_eq!(cam.owner, Backend::Libcamera);
    assert_eq!(cam.reason, Reason::LibcameraListed);
    assert_eq!(cam.seen_by, [Backend::Libcamera, Backend::V4l2]);
    assert_eq!(cam.id, "sysfs:/sys/d/imx");
}

#[test]
fn a_usb_camera_claimed_by_the_native_uvc_driver_appears_once_as_native() {
    // The native uvc driver serves the camera (a camera.frames/1 device, named in the board),
    // and a stale V4L2 node still shows the same USB port. One camera, native.
    let p = plan(
        vec![native("usb0", vec![lemnos("usb0"), usb("0000:00:14.0-2")])],
        vec![],
        vec![node("/dev/video6", vec![usb("0000:00:14.0-2")], false)],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(p.cameras.len(), 1, "{p:?}");
    assert_eq!(p.cameras[0].owner, Backend::Native);
    assert_eq!(p.cameras[0].reason, Reason::NativeChain);
    assert_eq!(p.cameras[0].seen_by, [Backend::Native, Backend::V4l2]);
}

#[test]
fn usb_ids_that_disagree_are_two_cameras() {
    let native_cam = native(
        "usb0",
        vec![Anchor::Usb {
            port: "0000:00:14.0-2".into(),
            ids: Some((0x046d, 0x0825)),
        }],
    );
    let other = node(
        "/dev/video6",
        vec![Anchor::Usb {
            port: "0000:00:14.0-2".into(),
            ids: Some((0x1234, 0x5678)),
        }],
        false,
    );
    let p = plan(vec![native_cam], vec![], vec![other], &Config::default()).unwrap();
    assert_eq!(p.cameras.len(), 2, "same port, different ids: {p:?}");
}

#[test]
fn a_backend_never_merges_its_own_listings() {
    // Two capture nodes of one device are two cameras, even though they share a sysfs device.
    let p = plan(
        vec![],
        vec![],
        vec![
            node("/dev/video0", vec![sysfs("/sys/d/pisp")], false),
            node("/dev/video1", vec![sysfs("/sys/d/pisp")], false),
        ],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(p.cameras.len(), 2, "{p:?}");
}

#[test]
fn a_virtual_node_is_not_chosen_by_default() {
    let p = plan(
        vec![],
        vec![],
        vec![node("/dev/video9", vec![], true)],
        &Config::default(),
    )
    .unwrap();
    assert!(p.cameras.is_empty(), "{p:?}");
    assert_eq!(p.skipped.len(), 1);
    assert_eq!(p.skipped[0].why, Skip::Virtual);
    assert_eq!(p.skipped[0].id, "video:/dev/video9");
}

#[test]
fn a_virtual_node_is_chosen_when_config_names_it() {
    let config = Config {
        virtual_nodes: vec!["video:/dev/video9".to_string()],
        ..Config::default()
    };
    let p = plan(
        vec![],
        vec![],
        vec![node("/dev/video9", vec![], true)],
        &config,
    )
    .unwrap();
    assert!(p.skipped.is_empty());
    assert_eq!(
        owner_of(&p, "video:/dev/video9"),
        (Backend::V4l2, Reason::VirtualNamed)
    );
}

#[test]
fn a_virtual_node_is_never_merged_with_a_real_camera() {
    // A loopback node that carries the same sysfs device as a native camera stays its own entry.
    let p = plan(
        vec![native(
            "sensor0",
            vec![lemnos("sensor0"), sysfs("/sys/d/csi0")],
        )],
        vec![],
        vec![node("/dev/video9", vec![sysfs("/sys/d/csi0")], true)],
        &Config::default(),
    )
    .unwrap();
    assert_eq!(p.cameras.len(), 1);
    assert_eq!(p.cameras[0].owner, Backend::Native);
    assert_eq!(p.skipped.len(), 1);
}

#[test]
fn a_global_force_applies_to_a_named_virtual_node_and_fails_loudly_when_it_cannot() {
    let config = Config {
        force: Some(Backend::Native),
        virtual_nodes: vec!["video:/dev/video9".to_string()],
        ..Config::default()
    };
    let err = plan(
        vec![],
        vec![],
        vec![node("/dev/video9", vec![], true)],
        &config,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        SelectError::ForcedMissing {
            backend: Backend::Native,
            ..
        }
    ));
}

struct Fixed(Result<Vec<Listed>, ListError>);

impl NativeLister for Fixed {
    fn list_native(&self) -> Result<Vec<Listed>, ListError> {
        self.0.clone()
    }
}
impl LibcameraLister for Fixed {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        self.0.clone()
    }
}
impl V4l2Lister for Fixed {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        self.0.clone()
    }
}

#[test]
fn select_takes_the_three_listers_and_reports_a_failed_listing() {
    let ok = Fixed(Ok(vec![]));
    let p = select(&ok, &ok, &ok, &Config::default()).unwrap();
    assert!(p.cameras.is_empty() && p.skipped.is_empty());

    let failing = Fixed(Err(ListError::new(Backend::Native, "lemnos not running")));
    let err = select(&failing, &ok, &ok, &Config::default()).unwrap_err();
    assert!(
        matches!(err, SelectError::List(ref e) if e.backend == Backend::Native),
        "{err:?}"
    );
}

#[test]
fn reasons_read_as_one_line() {
    assert_eq!(Reason::NativeChain.to_string(), "native chain lists it");
    assert_eq!(
        Reason::Forced {
            backend: Backend::V4l2,
            scope: Scope::Camera
        }
        .to_string(),
        "forced to v4l2 for this camera by config"
    );
    assert_eq!(
        Skip::Virtual.to_string(),
        "virtual V4L2 node; name it in config to use it"
    );
}
