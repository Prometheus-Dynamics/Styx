//! Video nodes as sysfs lists them. Reads `/sys/class/video4linux` only: the node itself is never
//! opened, so a virtual node (a loopback or a program's camera) is never started by listing it.
//! The sysfs `name` is the driver's card name; a node under `/sys/devices/virtual` is virtual.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use styx_select::{Anchor, Backend};

use crate::registry::Listing;

/// The sysfs class directory on a running system.
pub const CLASS_DIR: &str = "/sys/class/video4linux";
/// The device directory on a running system; nodes are `/dev/<name>`.
pub const DEV_DIR: &str = "/dev";

/// One `videoN` node, as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoNode {
    /// `<dev dir>/videoN`.
    pub path: PathBuf,
    /// The node's sysfs `name` (its card name), empty when it has none.
    pub name: String,
    /// The canonical sysfs device directory, when the node has one.
    pub device: Option<PathBuf>,
    /// Under `.../devices/virtual/` (the kernel's virtual devices): fed by another program, not a
    /// camera on the board.
    pub virtual_node: bool,
}

impl VideoNode {
    /// The listing the registry takes: the node's path and its sysfs device directory. The USB
    /// port needs the node's capability ioctl, which needs the node open, so it is not given.
    pub(crate) fn listing(&self) -> Listing {
        let mut anchors = vec![Anchor::VideoNode(self.path.clone())];
        if let Some(device) = &self.device {
            anchors.push(Anchor::Sysfs(device.clone()));
        }
        Listing {
            backend: Backend::V4l2,
            key: self.path.display().to_string(),
            anchors,
            virtual_node: self.virtual_node,
        }
    }
}

/// Every `videoN` under `class_dir`, nodes under `dev_dir`, sorted by number. A directory that
/// cannot be read is an error; a node whose attributes cannot be read is listed without them.
pub fn scan_video(class_dir: &Path, dev_dir: &Path) -> io::Result<Vec<VideoNode>> {
    let mut nodes = Vec::new();
    for entry in fs::read_dir(class_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_video(&name) {
            continue;
        }
        nodes.push(read_node(class_dir, dev_dir, &name));
    }
    nodes.sort_by_key(|node| number(&node.path));
    Ok(nodes)
}

/// One node by its name (`videoN`), when sysfs has it.
pub fn node(class_dir: &Path, dev_dir: &Path, name: &str) -> Option<VideoNode> {
    if !is_video(name) || !class_dir.join(name).exists() {
        return None;
    }
    Some(read_node(class_dir, dev_dir, name))
}

fn read_node(class_dir: &Path, dev_dir: &Path, name: &str) -> VideoNode {
    let dir = class_dir.join(name);
    let card = fs::read_to_string(dir.join("name"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let device = fs::canonicalize(dir.join("device")).ok();
    // The class entry is a link to the device's own directory, which is under `devices/virtual/`
    // for a virtual node. The `device` link can be gone (or dangle) once its program has stopped,
    // so the class entry and the link's target are both looked at.
    let class_target = fs::canonicalize(&dir).ok();
    let link_target = fs::read_link(dir.join("device")).ok();
    let virtual_node = [
        class_target.as_deref(),
        device.as_deref(),
        link_target.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(Path::to_str)
    .any(|p| p.contains("devices/virtual/"));
    VideoNode {
        path: dev_dir.join(name),
        name: card,
        device,
        virtual_node,
    }
}

fn is_video(name: &str) -> bool {
    name.strip_prefix("video")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn number(path: &Path) -> u64 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("video"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("styx-watch-sysfs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_video_nodes_by_sysfs_only_and_marks_virtual() {
        let root = fixture("list");
        let class = root.join("class");
        let real = root.join("devices/platform/vivid.0");
        let virt = root.join("devices/virtual/video4linux/video0/device");
        fs::create_dir_all(real.join("video4linux")).unwrap();
        fs::create_dir_all(&virt).unwrap();
        for (node, card, device) in [
            ("video1", "vivid-000-vid-cap", real.clone()),
            ("video0", "OBS Virtual Camera", virt.clone()),
        ] {
            fs::create_dir_all(class.join(node)).unwrap();
            fs::write(class.join(node).join("name"), format!("{card}\n")).unwrap();
            symlink(&device, class.join(node).join("device")).unwrap();
        }
        fs::create_dir_all(class.join("radio0")).unwrap();
        let dev = root.join("dev");
        let nodes = scan_video(&class, &dev).expect("scan");
        let names: Vec<_> = nodes.iter().map(|n| n.path.clone()).collect();
        assert_eq!(
            names,
            vec![dev.join("video0"), dev.join("video1")],
            "sorted, radio skipped"
        );
        assert!(nodes[0].virtual_node);
        assert!(!nodes[1].virtual_node);
        assert_eq!(nodes[1].name, "vivid-000-vid-cap");
        assert_eq!(
            node(&class, &dev, "video1").map(|n| n.name),
            Some("vivid-000-vid-cap".into())
        );
        assert_eq!(node(&class, &dev, "video9"), None);
        assert_eq!(node(&class, &dev, "radio0"), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_node_without_attributes_is_still_listed() {
        let root = fixture("bare");
        let class = root.join("class");
        fs::create_dir_all(class.join("video2")).unwrap();
        let nodes = scan_video(&class, Path::new("/dev")).expect("scan");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "");
        assert_eq!(nodes[0].device, None);
        assert!(!nodes[0].virtual_node);
        let _ = fs::remove_dir_all(root);
    }
}
