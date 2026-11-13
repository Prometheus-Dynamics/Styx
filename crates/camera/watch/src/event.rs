//! What the watcher reports: a camera arrived or left, with the identity styx-select uses.

use styx_select::{Anchor, Backend};

/// One listing of a camera by one backend: the backend's own key and the anchors that identify
/// the physical device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub backend: Backend,
    /// The backend's handle: a native camera name, a libcamera id, or a `/dev/videoN` path.
    pub key: String,
    pub anchors: Vec<Anchor>,
    /// A virtual V4L2 node (fed by another program). `styx-select` uses it only when config names
    /// the node; the watcher reports it so the consumer can decide the same way.
    pub virtual_node: bool,
}

/// One physical camera: the listings of it, one per backend at most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Camera {
    pub members: Vec<Member>,
}

impl Camera {
    /// The name a config file uses for this camera: the best-ranked anchor's name, ranked as
    /// [`Anchor::name`] documents (Lemnos device, then USB port, then sysfs, then libcamera id,
    /// then node path). Matches what `styx-select` names the camera.
    pub fn name(&self) -> String {
        self.anchors()
            .min_by_key(|anchor| rank(anchor))
            .map(Anchor::name)
            .unwrap_or_default()
    }

    /// Every anchor of every listing.
    pub fn anchors(&self) -> impl Iterator<Item = &Anchor> {
        self.members.iter().flat_map(|m| m.anchors.iter())
    }

    /// The listing of this camera by `backend`, if that backend lists it.
    pub fn by(&self, backend: Backend) -> Option<&Member> {
        self.members.iter().find(|m| m.backend == backend)
    }
}

fn rank(anchor: &Anchor) -> u8 {
    match anchor {
        Anchor::Lemnos(_) => 0,
        Anchor::Usb { .. } => 1,
        Anchor::Sysfs(_) => 2,
        Anchor::Libcamera(_) => 3,
        Anchor::VideoNode(_) => 4,
    }
}

/// Whether a camera appeared or went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Arrived,
    Left,
}

/// A camera appearing or leaving. On `Left` the camera is the one that went, with the listings it
/// had when it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub kind: Kind,
    pub camera: Camera,
}
