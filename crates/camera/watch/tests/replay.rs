//! The V4L2 path with recorded kernel uevents, fed to the watcher as if read from the netlink
//! socket. The sysfs tree is a fixture in a temporary directory, so no real device is touched
//! and no node is opened. Removal of a real node needs root, so the removal is replayed here.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use styx_watch::{Event, Kind, Watch, WatchConfig};

// Messages in the kernel's format, as the kernel sends them on the kernel's group.
const ADD_VID_CAP: &[u8] = b"add@/devices/platform/vivid.0/video4linux/video1\0\
ACTION=add\0DEVPATH=/devices/platform/vivid.0/video4linux/video1\0SUBSYSTEM=video4linux\0\
DEVNAME=video1\0MAJOR=81\0MINOR=1\0SEQNUM=2140\0";
const REMOVE_VID_CAP: &[u8] = b"remove@/devices/platform/vivid.0/video4linux/video1\0\
ACTION=remove\0DEVPATH=/devices/platform/vivid.0/video4linux/video1\0SUBSYSTEM=video4linux\0\
DEVNAME=video1\0MAJOR=81\0MINOR=1\0SEQNUM=2141\0";
const BIND_USB: &[u8] = b"bind@/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0\0\
ACTION=bind\0DEVPATH=/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0\0SUBSYSTEM=usb\0\
DRIVER=uvcvideo\0SEQNUM=2142\0";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("styx-watch-replay-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("class")).unwrap();
        fs::create_dir_all(root.join("dev")).unwrap();
        Self { root }
    }

    fn class(&self) -> PathBuf {
        self.root.join("class")
    }

    fn dev(&self) -> PathBuf {
        self.root.join("dev")
    }

    /// Creates `class/<node>` as sysfs shows a vivid capture node.
    fn add_node(&self, node: &str, card: &str, device: &Path) {
        let dir = self.class().join(node);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), format!("{card}\n")).unwrap();
        symlink(device, dir.join("device")).unwrap();
    }

    fn remove_node(&self, node: &str) {
        fs::remove_dir_all(self.class().join(node)).unwrap();
    }

    fn device(&self) -> PathBuf {
        let dir = self.root.join("devices/platform/vivid.0");
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn watch(fixture: &Fixture) -> Watch {
    Watch::new(WatchConfig {
        client: None,
        class_dir: fixture.class(),
        dev_dir: fixture.dev(),
        netlink: false,
        libcamera: false,
    })
}

fn drain(watch: &mut Watch) -> Vec<Event> {
    let mut out = Vec::new();
    while let Some(event) = watch.poll(Duration::from_millis(5)) {
        out.push(event);
    }
    out
}

#[test]
fn recorded_add_and_remove_give_arrival_then_departure_with_no_open_node() {
    let fixture = Fixture::new("order");
    let device = fixture.device();
    let mut watch = watch(&fixture);
    assert!(
        drain(&mut watch).is_empty(),
        "nothing on the fixture board yet"
    );

    // The kernel adds video1: sysfs has it, the event names it.
    fixture.add_node("video1", "vivid-000-vid-cap", &device);
    watch.feed_uevent(ADD_VID_CAP);
    let added = drain(&mut watch);
    assert_eq!(added.len(), 1, "{added:?}");
    assert_eq!(added[0].kind, Kind::Arrived);
    assert_eq!(
        added[0].camera.members[0].key,
        fixture.dev().join("video1").display().to_string()
    );

    // A message for another subsystem changes nothing.
    watch.feed_uevent(BIND_USB);
    assert!(drain(&mut watch).is_empty());

    // The same add again is not a second camera.
    watch.feed_uevent(ADD_VID_CAP);
    assert!(drain(&mut watch).is_empty());

    // The kernel removes video1: the event takes it away.
    fixture.remove_node("video1");
    watch.feed_uevent(REMOVE_VID_CAP);
    let removed = drain(&mut watch);
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0].kind, Kind::Left);
    assert!(watch.cameras().is_empty());

    // Added again: arrives again, after it left.
    fixture.add_node("video1", "vivid-000-vid-cap", &device);
    watch.feed_uevent(ADD_VID_CAP);
    let again = drain(&mut watch);
    assert_eq!(
        again.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![Kind::Arrived]
    );
    assert_eq!(watch.lost(), 0);
}

#[test]
fn a_node_on_the_board_at_start_arrives_on_the_first_poll() {
    let fixture = Fixture::new("start");
    let device = fixture.device();
    fixture.add_node("video1", "vivid-000-vid-cap", &device);
    fixture.add_node("video0", "OBS Virtual Camera", &device);
    let mut watch = watch(&fixture);
    let events = drain(&mut watch);
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events.iter().all(|e| e.kind == Kind::Arrived));
}
