//! The watcher on this machine's V4L2 nodes, read from sysfs. The node named `vivid-000-vid-cap`
//! is matched by its sysfs name only; no node is opened by this test or by the watcher. The
//! virtual node (any node under `/sys/devices/virtual`) is seen and marked virtual, never opened.

use std::path::Path;
use std::time::{Duration, Instant};

use styx_select::Anchor;
use styx_watch::{Kind, SourceStatus, Watch, WatchConfig, WatchStatus, scan_video};

const VIVID_CAP: &str = "vivid-000-vid-cap";

/// The `/dev` node whose sysfs name is `name`, from sysfs alone.
fn node_named(name: &str) -> Option<String> {
    scan_video(Path::new("/sys/class/video4linux"), Path::new("/dev"))
        .ok()?
        .into_iter()
        .find(|n| n.name == name)
        .map(|n| n.path.display().to_string())
}

#[test]
fn vivid_capture_node_arrives_from_sysfs_and_the_socket_listens() {
    let Some(vivid) = node_named(VIVID_CAP) else {
        println!("SKIP: no V4L2 node named {VIVID_CAP} in sysfs (the vivid driver is not loaded)");
        return;
    };
    let mut watch = Watch::new(WatchConfig::system(None));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut arrived = Vec::new();
    while Instant::now() < deadline {
        if let Some(event) = watch.poll(Duration::from_millis(100)) {
            assert_eq!(
                event.kind,
                Kind::Arrived,
                "nothing leaves in two seconds: {event:?}"
            );
            arrived.push(event);
        }
    }
    let vivid_event = arrived
        .iter()
        .find(|e| e.camera.members.iter().any(|m| m.key == vivid))
        .unwrap_or_else(|| panic!("no arrival for {vivid}; arrivals: {arrived:?}"));
    assert!(
        vivid_event
            .camera
            .anchors()
            .any(|a| matches!(a, Anchor::Sysfs(_)))
    );
    let member = vivid_event
        .camera
        .by(styx_select::Backend::V4l2)
        .expect("a V4L2 listing");
    assert!(!member.virtual_node, "the capture node is not virtual");
    // A virtual node (if this machine has one) arrives marked virtual, and is never opened here.
    let virtual_seen = arrived
        .iter()
        .filter(|e| {
            e.camera
                .by(styx_select::Backend::V4l2)
                .is_some_and(|m| m.virtual_node)
        })
        .count();
    println!("virtual V4L2 nodes arrived and marked: {virtual_seen}");
    let status: WatchStatus = watch.status();
    println!("vivid node {vivid} arrived; v4l2 socket: {:?}", status.v4l2);
    // Listening for two seconds with no error: the socket is live, or it said why it could not open.
    if let SourceStatus::Retrying { reason, .. } = &status.v4l2 {
        println!("SKIP-SOCKET: the uevent socket could not open here: {reason}");
    } else {
        assert!(matches!(
            status.v4l2,
            SourceStatus::Live | SourceStatus::Waiting
        ));
    }
}
