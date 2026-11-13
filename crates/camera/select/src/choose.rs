//! Choosing one owner per camera.
//!
//! Rules, in order:
//! 1. Clustering. A native listing starts a camera. A libcamera listing joins the camera that
//!    shares an anchor with it, if that camera has no libcamera listing yet; otherwise it starts
//!    one. A V4L2 listing joins the same way; a virtual V4L2 node never joins anything. A backend
//!    never lists one camera twice, so two listings from the same backend are always two cameras.
//! 2. Virtual nodes. A virtual V4L2 node is skipped unless config names it (by any of its anchor
//!    names, in `virtual_nodes` or in `per_camera`).
//! 3. Forced. `per_camera` wins over `force`, and a forced choice wins over the default. Forcing a
//!    backend that does not list the camera is an error naming the camera and the backend.
//!    Config naming a camera nobody lists is an error too.
//! 4. Default. Native if it lists the camera, else libcamera, else V4L2.
//!
//! Nothing here opens a device. The listers may probe: the V4L2 lister lists virtual nodes from
//! sysfs without opening them, and fully probes each physical node once. The choice itself only
//! compares anchors.

use std::fmt;

use crate::listing::{
    Anchor, Backend, LibcameraLister, ListError, Listed, NativeLister, V4l2Lister,
};

/// Config for the choice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Forces every selected camera to this backend. A camera the backend does not list is an
    /// error.
    pub force: Option<Backend>,
    /// Forces one camera. The name is any of the camera's anchor names (for example
    /// `lemnos:sensor0`, `usb:0000:00:14.0-2`, `libcamera:...`). Wins over `force`.
    pub per_camera: Vec<(String, Backend)>,
    /// Virtual V4L2 nodes to use, by any of their anchor names. Without a name here a virtual
    /// node is never chosen.
    pub virtual_nodes: Vec<String>,
}

/// Where a forced choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `Config::force`.
    Global,
    /// `Config::per_camera`.
    Camera,
}

/// Why a camera has the owner it has. Displays as one line for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The native backend lists it (a native chain, or a `camera.frames/1` USB camera).
    NativeChain,
    /// No native listing; libcamera lists it.
    LibcameraListed,
    /// No native or libcamera listing; V4L2 lists it.
    V4l2Listed,
    /// Config forces the owner.
    Forced { backend: Backend, scope: Scope },
    /// A virtual V4L2 node that config names.
    VirtualNamed,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::NativeChain => f.write_str("native chain lists it"),
            Reason::LibcameraListed => f.write_str("no native chain; libcamera lists it"),
            Reason::V4l2Listed => {
                f.write_str("no native chain or libcamera listing; V4L2 lists it")
            }
            Reason::Forced {
                backend,
                scope: Scope::Global,
            } => write!(f, "forced to {backend} by config"),
            Reason::Forced {
                backend,
                scope: Scope::Camera,
            } => write!(f, "forced to {backend} for this camera by config"),
            Reason::VirtualNamed => f.write_str("virtual V4L2 node named in config"),
        }
    }
}

/// One camera and its one owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    /// The camera's name: its best stable anchor (Lemnos device, then USB port, then sysfs device,
    /// then libcamera id, then node path).
    pub id: String,
    /// The one backend that opens it.
    pub owner: Backend,
    /// The owner's handle for the camera (see [`Listed::key`]).
    pub key: String,
    pub reason: Reason,
    /// Every backend that lists the camera, in preference order.
    pub seen_by: Vec<Backend>,
}

/// Why a camera was left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A virtual V4L2 node that config does not name.
    Virtual,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Skip::Virtual => f.write_str("virtual V4L2 node; name it in config to use it"),
        }
    }
}

/// A camera that was listed but not chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub id: String,
    pub why: Skip,
}

/// The result: the cameras with their owners, and what was left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub cameras: Vec<Selected>,
    pub skipped: Vec<Skipped>,
}

/// Why no plan was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// A backend could not list its cameras.
    List(ListError),
    /// Config forces `backend` for `camera`, and `backend` does not list it.
    ForcedMissing {
        camera: String,
        backend: Backend,
        seen_by: Vec<Backend>,
    },
    /// Config names `camera`, and no backend lists it.
    UnknownCamera { camera: String },
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectError::List(e) => write!(f, "{e}"),
            SelectError::ForcedMissing {
                camera,
                backend,
                seen_by,
            } => {
                let seen: Vec<String> = seen_by.iter().map(Backend::to_string).collect();
                let seen = if seen.is_empty() {
                    "no backend".to_string()
                } else {
                    seen.join(", ")
                };
                write!(
                    f,
                    "camera {camera} is forced to {backend}, but {backend} does not list it (listed by: {seen})"
                )
            }
            SelectError::UnknownCamera { camera } => {
                write!(f, "config names camera {camera}, but no backend lists it")
            }
        }
    }
}

impl std::error::Error for SelectError {}

/// Lists every backend and makes the plan.
pub fn select(
    native: &dyn NativeLister,
    libcamera: &dyn LibcameraLister,
    v4l2: &dyn V4l2Lister,
    config: &Config,
) -> Result<Plan, SelectError> {
    let native_list = native.list_native().map_err(SelectError::List)?;
    let libcamera_list = libcamera.list_libcamera().map_err(SelectError::List)?;
    let v4l2_list = v4l2.list_v4l2().map_err(SelectError::List)?;
    plan(native_list, libcamera_list, v4l2_list, config)
}

/// One physical camera: the listing each backend made of it.
struct Cluster {
    anchors: Vec<Anchor>,
    listed: [Option<Listed>; 3],
}

impl Cluster {
    fn new(backend: Backend, item: Listed) -> Self {
        let mut listed: [Option<Listed>; 3] = [None, None, None];
        let anchors = item.anchors.clone();
        listed[backend.index()] = Some(item);
        Self { anchors, listed }
    }

    fn add(&mut self, backend: Backend, item: Listed) {
        self.anchors.extend(item.anchors.iter().cloned());
        self.listed[backend.index()] = Some(item);
    }

    fn joins(&self, backend: Backend, item: &Listed) -> bool {
        self.listed[backend.index()].is_none()
            && self
                .anchors
                .iter()
                .any(|a| item.anchors.iter().any(|b| a.same_device(b)))
    }

    fn is_virtual(&self) -> bool {
        self.listed[Backend::V4l2.index()]
            .as_ref()
            .is_some_and(|l| l.virtual_node)
    }

    fn names_include(&self, names: &[&str]) -> bool {
        self.anchors
            .iter()
            .any(|a| names.contains(&a.name().as_str()))
    }

    fn has_name(&self, name: &str) -> bool {
        self.anchors.iter().any(|a| a.name() == name)
    }

    fn seen_by(&self) -> Vec<Backend> {
        Backend::ALL
            .into_iter()
            .filter(|b| self.listed[b.index()].is_some())
            .collect()
    }

    fn id(&self) -> String {
        match self.anchors.iter().min_by_key(|a| a.stable_rank()) {
            Some(anchor) => anchor.name(),
            None => {
                let first = self.seen_by()[0];
                let key = self.listed[first.index()]
                    .as_ref()
                    .map_or("", |l| l.key.as_str());
                format!("{first}:{key}")
            }
        }
    }
}

/// Makes the plan from what each backend lists. See the module documentation for the rules.
pub fn plan(
    native: Vec<Listed>,
    libcamera: Vec<Listed>,
    v4l2: Vec<Listed>,
    config: &Config,
) -> Result<Plan, SelectError> {
    let mut clusters: Vec<Cluster> = Vec::new();
    for (backend, list) in [
        (Backend::Native, native),
        (Backend::Libcamera, libcamera),
        (Backend::V4l2, v4l2),
    ] {
        for item in list {
            let joined = if item.virtual_node {
                None
            } else {
                clusters.iter().position(|c| c.joins(backend, &item))
            };
            match joined {
                Some(i) => clusters[i].add(backend, item),
                None => clusters.push(Cluster::new(backend, item)),
            }
        }
    }

    for (name, _) in &config.per_camera {
        if !clusters.iter().any(|c| c.has_name(name)) {
            return Err(SelectError::UnknownCamera {
                camera: name.clone(),
            });
        }
    }
    for name in &config.virtual_nodes {
        if !clusters.iter().any(|c| c.has_name(name)) {
            return Err(SelectError::UnknownCamera {
                camera: name.clone(),
            });
        }
    }

    let mut out = Plan::default();
    for cluster in &clusters {
        let id = cluster.id();
        let per_camera = config
            .per_camera
            .iter()
            .find(|(name, _)| cluster.has_name(name))
            .map(|(_, backend)| *backend);
        let named = per_camera.is_some()
            || cluster.names_include(
                &config
                    .virtual_nodes
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            );
        if cluster.is_virtual() && !named {
            out.skipped.push(Skipped {
                id,
                why: Skip::Virtual,
            });
            continue;
        }

        let seen_by = cluster.seen_by();
        let forced = match (per_camera, config.force) {
            (Some(backend), _) => Some((backend, Scope::Camera)),
            (None, Some(backend)) => Some((backend, Scope::Global)),
            (None, None) => None,
        };
        let (owner, reason) = match forced {
            Some((backend, scope)) => {
                if cluster.listed[backend.index()].is_none() {
                    return Err(SelectError::ForcedMissing {
                        camera: id,
                        backend,
                        seen_by,
                    });
                }
                (backend, Reason::Forced { backend, scope })
            }
            None if cluster.listed[Backend::Native.index()].is_some() => {
                (Backend::Native, Reason::NativeChain)
            }
            None if cluster.listed[Backend::Libcamera.index()].is_some() => {
                (Backend::Libcamera, Reason::LibcameraListed)
            }
            None if cluster.is_virtual() => (Backend::V4l2, Reason::VirtualNamed),
            None => (Backend::V4l2, Reason::V4l2Listed),
        };
        let key = cluster.listed[owner.index()]
            .as_ref()
            .map_or_else(String::new, |l| l.key.clone());
        out.cameras.push(Selected {
            id,
            owner,
            key,
            reason,
            seen_by,
        });
    }
    Ok(out)
}
