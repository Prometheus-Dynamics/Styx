//! Node selection shared by the V4L2 tests. A node under `/sys/devices/virtual` is a loopback or
//! virtual camera fed by another program (OBS, on the dev PC). Opening one can start that program's
//! output, so these tests decide from sysfs first and never open such a node.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use styx_v4l2::{Probe, probe_node};

/// Whether the video node `video` (for example `video0`) is a virtual camera, by its sysfs path.
pub fn is_virtual(video: &str) -> bool {
    fs::canonicalize(format!("/sys/class/video4linux/{video}/device"))
        .map(|path| path.starts_with("/sys/devices/virtual"))
        .unwrap_or(false)
}

/// The sysfs name of a video node (its `name` file), or empty when it has none.
pub fn sysfs_name(video: &str) -> String {
    fs::read_to_string(format!("/sys/class/video4linux/{video}/name"))
        .map(|name| name.trim().to_string())
        .unwrap_or_default()
}

/// Every `/dev/videoN` node, sorted by number, that is not virtual. Virtual nodes are reported on
/// stderr and never opened.
pub fn physical_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<(u32, String)> = fs::read_dir("/dev")
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    let number = name.strip_prefix("video")?.parse::<u32>().ok()?;
                    Some((number, name))
                })
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    let mut out = Vec::new();
    for (_, name) in nodes {
        if is_virtual(&name) {
            eprintln!(
                "virtual node /dev/{name} ({}) is not opened by these tests",
                sysfs_name(&name)
            );
            continue;
        }
        out.push(Path::new("/dev").join(name));
    }
    out
}

/// A probe of the physical capture nodes only. Errors are kept per node.
pub fn physical_probe() -> Probe {
    let mut out = Probe::default();
    for path in physical_nodes() {
        match probe_node(&path) {
            Ok(Some(device)) => out.devices.push(device),
            Ok(None) => {}
            Err(e) => out.errors.push(format!("{}: {e}", path.display())),
        }
    }
    out
}
