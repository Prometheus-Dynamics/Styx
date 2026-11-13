//! Node selection shared by the V4L2 tests. A virtual node is a loopback or virtual camera fed by
//! another program (OBS, on the dev PC). Opening one can start that program's output, so these tests
//! list from sysfs and never open a virtual node. Nodes are chosen by their sysfs name, never by
//! number.

#![allow(dead_code)]

use std::path::PathBuf;

use styx_v4l2::{Device, Node, probe, probe_node};

/// The `videoN` nodes that are not virtual, from sysfs. Virtual nodes are reported on stderr and
/// never opened.
pub fn physical_nodes() -> Vec<Node> {
    let mut out = Vec::new();
    for node in probe().nodes {
        let is_video = node
            .path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("video"));
        if !is_video {
            continue;
        }
        if node.virtual_node {
            eprintln!(
                "virtual node {} ({}) is not opened by these tests",
                node.path.display(),
                node.name
            );
            continue;
        }
        out.push(node);
    }
    out
}

/// Fully probes every physical capture node. Errors are printed and the node is left out.
pub fn physical_devices() -> Vec<Device> {
    let mut out = Vec::new();
    for node in physical_nodes() {
        match probe_node(&node.path) {
            Ok(Some(device)) => out.push(device),
            Ok(None) => {}
            Err(e) => println!("probe error: {}: {e}", node.path.display()),
        }
    }
    out
}

/// The path of the first physical capture node with this sysfs name, if any.
pub fn path_named(name: &str) -> Option<PathBuf> {
    physical_nodes()
        .into_iter()
        .find(|node| node.name == name)
        .map(|node| node.path)
}
