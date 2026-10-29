use super::*;

/// The properties a V4L2 node of a USB camera reports (`styx_v4l2::probe_devices`).
fn usb_node(path: &str, bus: &str, ids: (&str, &str), card: &str) -> Vec<(String, String)> {
    vec![
        ("path".into(), path.into()),
        ("name".into(), card.into()),
        ("driver".into(), "uvcvideo".into()),
        ("card".into(), card.into()),
        ("bus".into(), bus.into()),
        ("usb_vendor".into(), ids.0.into()),
        ("usb_product".into(), ids.1.into()),
    ]
}

#[test]
fn a_usb_camera_is_keyed_by_its_port_and_vendor_product() {
    let keys = derive_keys(
        "/dev/video0",
        &usb_node(
            "/dev/video0",
            "usb-xhci-hcd.1-1.1.1",
            ("046d", "0825"),
            "UVC Camera (046d:0825)",
        ),
    );
    assert_eq!(
        keys,
        vec![
            "usb-xhci-hcd.1-1.1.1".to_string(),
            "046d:0825@usb-xhci-hcd.1-1.1.1".to_string(),
        ]
    );
}

#[test]
fn a_usb_camera_has_no_key_another_camera_shares() {
    let c270 = derive_keys(
        "/dev/video0",
        &usb_node(
            "/dev/video0",
            "usb-xhci-hcd.1-1.1.1",
            ("046d", "0825"),
            "UVC Camera (046d:0825)",
        ),
    );
    let creative = derive_keys(
        "/dev/video2",
        &usb_node(
            "/dev/video2",
            "usb-xhci-hcd.1-1.1.3",
            ("041e", "40a3"),
            "Creative Live! Cam Sync V3: Cre",
        ),
    );
    for shared in ["uvcvideo", "046d:0825", "UVC Camera (046d:0825)"] {
        assert!(
            !creative.iter().any(|k| k == shared),
            "{shared} in {creative:?}"
        );
    }
    assert!(c270.iter().all(|k| !creative.contains(k)));
}

#[test]
fn non_usb_nodes_keep_their_identity_keys() {
    let props = vec![
        ("path".into(), "/dev/video4".into()),
        ("card".into(), "imx708".into()),
        ("bus".into(), "platform:1f00128000.csi".into()),
    ];
    let keys = derive_keys("/dev/video4", &props);
    assert!(keys.contains(&"imx708".to_string()));
    assert!(keys.contains(&"platform:1f00128000.csi".to_string()));
}

#[test]
#[cfg(any(feature = "v4l2", feature = "libcamera"))]
fn display_comes_from_the_nodes_own_card() {
    let c270 = usb_node(
        "/dev/video0",
        "usb-xhci-hcd.1-1.1.1",
        ("046d", "0825"),
        "UVC Camera (046d:0825)",
    );
    assert_eq!(pick_display_id("/dev/video0", &c270), "046d:0825");
    let creative = usb_node(
        "/dev/video2",
        "usb-xhci-hcd.1-1.1.3",
        ("041e", "40a3"),
        "Creative Live! Cam Sync V3: Cre",
    );
    assert_eq!(
        pick_display_id("/dev/video2", &creative),
        "Creative Live! Cam Sync V3: Cre"
    );
}

#[cfg(feature = "v4l2")]
mod merged {
    use super::*;
    use crate::{BackendHandle, BackendKind, ProbedBackend, ProbedDevice};
    use styx_capture::prelude::CaptureDescriptor;

    /// A V4L2 capture (or metadata) node of a USB camera, as `merge_backend` receives it.
    fn node(path: &str, bus: &str, ids: (&str, &str), card: &str) -> (String, ProbedBackend) {
        let backend = ProbedBackend {
            kind: BackendKind::V4l2,
            handle: BackendHandle::V4l2 { path: path.into() },
            descriptor: CaptureDescriptor {
                modes: Vec::new(),
                controls: Vec::new(),
            },
            properties: usb_node(path, bus, ids, card),
        };
        (path.to_string(), backend)
    }

    fn merged(nodes: Vec<(String, ProbedBackend)>) -> Vec<ProbedDevice> {
        let mut devices = Vec::new();
        for (id, backend) in nodes {
            crate::merge_backend(&mut devices, id, backend);
        }
        devices
    }

    const C270_CARD: &str = "UVC Camera (046d:0825)";
    const CREATIVE_CARD: &str = "Creative Live! Cam Sync V3: Cre";

    #[test]
    fn two_different_cameras_on_one_hub_stay_two_devices() {
        let devices = merged(vec![
            node(
                "/dev/video0",
                "usb-xhci-hcd.1-1.1.1",
                ("046d", "0825"),
                C270_CARD,
            ),
            node(
                "/dev/video2",
                "usb-xhci-hcd.1-1.1.3",
                ("041e", "40a3"),
                CREATIVE_CARD,
            ),
        ]);
        assert_eq!(devices.len(), 2);
        let (c270, creative) = (&devices[0], &devices[1]);
        assert_eq!(c270.identity.display, "046d:0825");
        assert_eq!(creative.identity.display, CREATIVE_CARD);
        assert!(
            c270.identity
                .keys
                .iter()
                .all(|k| !creative.identity.keys.contains(k)),
            "{:?} and {:?} share a key",
            c270.identity.keys,
            creative.identity.keys
        );
        assert!(!creative.identity.keys.iter().any(|k| k == "uvcvideo"));
    }

    #[test]
    fn identical_cameras_on_different_ports_stay_two_devices() {
        let devices = merged(vec![
            node(
                "/dev/video0",
                "usb-xhci-hcd.1-1.1.1",
                ("046d", "0825"),
                C270_CARD,
            ),
            node(
                "/dev/video4",
                "usb-xhci-hcd.1-1.2",
                ("046d", "0825"),
                C270_CARD,
            ),
        ]);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].backends.len(), 1);
        assert_eq!(devices[1].backends.len(), 1);
        assert!(
            devices[0]
                .identity
                .keys
                .iter()
                .all(|k| !devices[1].identity.keys.contains(k))
        );
    }

    #[test]
    fn a_cameras_metadata_node_groups_with_its_capture_node() {
        let devices = merged(vec![
            node(
                "/dev/video0",
                "usb-xhci-hcd.1-1.1.1",
                ("046d", "0825"),
                C270_CARD,
            ),
            node(
                "/dev/video1",
                "usb-xhci-hcd.1-1.1.1",
                ("046d", "0825"),
                C270_CARD,
            ),
            node(
                "/dev/video2",
                "usb-xhci-hcd.1-1.1.3",
                ("041e", "40a3"),
                CREATIVE_CARD,
            ),
            node(
                "/dev/video3",
                "usb-xhci-hcd.1-1.1.3",
                ("041e", "40a3"),
                CREATIVE_CARD,
            ),
        ]);
        assert_eq!(devices.len(), 2);
        let paths = |d: &ProbedDevice| {
            d.backends
                .iter()
                .filter_map(|b| match &b.handle {
                    BackendHandle::V4l2 { path } => Some(path.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&devices[0]), ["/dev/video0", "/dev/video1"]);
        assert_eq!(paths(&devices[1]), ["/dev/video2", "/dev/video3"]);
    }
}
