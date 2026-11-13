//! The listers over the real backends. Each one lists; none opens a camera for good.
//!
//! The V4L2 lister calls `styx_v4l2::probe`, which opens every video node to read it. That is
//! the probe's behaviour today, virtual nodes included; it should not open virtual nodes by
//! default (a change to `styx-v4l2`, not made here).

use std::fs;
use std::path::{Path, PathBuf};

use styx_lemnos::{Client, FRAMES_TAG, SENSOR_TAG};

use crate::listing::{
    Anchor, Backend, LibcameraLister, ListError, Listed, NativeLister, V4l2Lister,
};

#[derive(Debug)]
/// The native backend over a Lemnos client: every camera chain (`camera.sensor/1`) and every
/// `camera.frames/1` device.
pub struct Lemnos<'a> {
    pub client: &'a Client,
}

impl NativeLister for Lemnos<'_> {
    fn list_native(&self) -> Result<Vec<Listed>, ListError> {
        let devices = self
            .client
            .list()
            .map_err(|e| ListError::new(Backend::Native, e.to_string()))?;
        let mut out: Vec<Listed> = styx_native::cameras(&devices)
            .cameras
            .iter()
            .map(|camera| Listed {
                key: camera.name().to_string(),
                anchors: vec![Anchor::Lemnos(camera.name().to_string())],
                virtual_node: false,
            })
            .collect();
        out.extend(
            devices
                .iter()
                .filter(|d| d.tags.iter().any(|t| t == FRAMES_TAG))
                .filter(|d| !d.tags.iter().any(|t| t == SENSOR_TAG))
                .map(|d| Listed {
                    key: d.name.clone(),
                    anchors: vec![Anchor::Lemnos(d.name.clone())],
                    virtual_node: false,
                }),
        );
        Ok(out)
    }
}

/// libcamera's cameras. No cameras (not an error) when libcamera is not available.
#[derive(Debug)]
pub struct Libcamera;

impl LibcameraLister for Libcamera {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError> {
        match styx_libcamera::list_cameras() {
            Ok(cameras) => Ok(cameras
                .into_iter()
                .map(|c| Listed {
                    anchors: vec![Anchor::Libcamera(c.id.clone())],
                    key: c.id,
                    virtual_node: false,
                })
                .collect()),
            Err(styx_libcamera::Error::Unavailable(_)) => Ok(Vec::new()),
            Err(e) => Err(ListError::new(Backend::Libcamera, e.to_string())),
        }
    }
}

/// The V4L2 capture nodes, from `styx_v4l2::probe`.
#[derive(Debug)]
pub struct Probe;

impl V4l2Lister for Probe {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError> {
        Ok(styx_v4l2::probe()
            .devices
            .into_iter()
            .map(|device| {
                let mut anchors = vec![Anchor::VideoNode(device.path.clone())];
                if let Some(sysfs) = sysfs_device(&device.path) {
                    anchors.push(Anchor::Sysfs(sysfs));
                }
                if let Some(port) = device.bus_info.strip_prefix("usb-") {
                    anchors.push(Anchor::Usb {
                        port: port.to_string(),
                        ids: None,
                    });
                }
                Listed {
                    key: device.path.display().to_string(),
                    anchors,
                    virtual_node: device.virtual_node,
                }
            })
            .collect())
    }
}

/// The canonical sysfs device directory of a video node (`/sys/class/video4linux/<node>/device`).
/// Reads sysfs only; the node is not opened.
fn sysfs_device(node: &Path) -> Option<PathBuf> {
    let name = node.file_name()?;
    fs::canonicalize(
        PathBuf::from("/sys/class/video4linux")
            .join(name)
            .join("device"),
    )
    .ok()
}
