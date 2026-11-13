//! What each backend lists, and how one physical camera is recognised across backends.
//!
//! A backend lists its cameras as [`Listed`]: a key the backend itself understands (the native
//! camera's name, libcamera's camera id, the V4L2 node path) and the [`Anchor`]s that identify the
//! physical device. The chooser never opens anything; it only compares anchors.

use std::fmt;
use std::path::PathBuf;

/// The three owners, in the order they are preferred: native, then libcamera, then V4L2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Backend {
    Native,
    Libcamera,
    V4l2,
}

impl Backend {
    /// All three backends, in preference order.
    pub const ALL: [Backend; 3] = [Backend::Native, Backend::Libcamera, Backend::V4l2];

    pub(crate) fn index(self) -> usize {
        match self {
            Backend::Native => 0,
            Backend::Libcamera => 1,
            Backend::V4l2 => 2,
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Backend::Native => "native",
            Backend::Libcamera => "libcamera",
            Backend::V4l2 => "v4l2",
        })
    }
}

/// A piece of evidence that two listings are the same physical camera.
///
/// Two anchors match only when they are the same kind and:
/// - `Lemnos`: the same device name. The name is the device's name in the board file, unique on
///   one board.
/// - `Usb`: the same bus and port (V4L2's `bus_info` after its `usb-` prefix, for example
///   `0000:00:14.0-2`), and, when both sides give them, the same vendor and product ids.
/// - `Sysfs`: the same canonical sysfs device directory. A V4L2 node's device directory is shared
///   by every node of one media device.
/// - `VideoNode`: the same device node path. Used for naming; never equal across backends, since
///   no backend lists another's node.
/// - `Libcamera`: the same libcamera camera id. Only libcamera lists these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    Lemnos(String),
    Usb {
        port: String,
        ids: Option<(u16, u16)>,
    },
    Sysfs(PathBuf),
    VideoNode(PathBuf),
    Libcamera(String),
}

impl Anchor {
    /// Whether two anchors name the same physical device (see the type's documentation).
    pub fn same_device(&self, other: &Anchor) -> bool {
        match (self, other) {
            (Anchor::Lemnos(a), Anchor::Lemnos(b)) => a == b,
            (Anchor::Usb { port: a, ids: ia }, Anchor::Usb { port: b, ids: ib }) => {
                a == b
                    && match (ia, ib) {
                        (Some(x), Some(y)) => x == y,
                        _ => true,
                    }
            }
            (Anchor::Sysfs(a), Anchor::Sysfs(b)) => a == b,
            (Anchor::VideoNode(a), Anchor::VideoNode(b)) => a == b,
            (Anchor::Libcamera(a), Anchor::Libcamera(b)) => a == b,
            _ => false,
        }
    }

    /// The name a config file uses for this anchor, for example `lemnos:sensor0` or
    /// `usb:0000:00:14.0-2`. Rank for [`Anchor::stable_rank`] decides which name a camera shows.
    pub fn name(&self) -> String {
        match self {
            Anchor::Lemnos(n) => format!("lemnos:{n}"),
            Anchor::Usb { port, .. } => format!("usb:{port}"),
            Anchor::Sysfs(p) => format!("sysfs:{}", p.display()),
            Anchor::VideoNode(p) => format!("video:{}", p.display()),
            Anchor::Libcamera(id) => format!("libcamera:{id}"),
        }
    }

    /// Lower ranks name a camera first: Lemnos device, then USB port, then sysfs device, then
    /// libcamera id, then node path (node paths are the least stable).
    pub(crate) fn stable_rank(&self) -> u8 {
        match self {
            Anchor::Lemnos(_) => 0,
            Anchor::Usb { .. } => 1,
            Anchor::Sysfs(_) => 2,
            Anchor::Libcamera(_) => 3,
            Anchor::VideoNode(_) => 4,
        }
    }
}

/// One camera a backend lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The backend's own handle for the camera: the native camera name, libcamera's camera id or
    /// the V4L2 node path. The caller opens the camera with it.
    pub key: String,
    /// The evidence that identifies the physical device.
    pub anchors: Vec<Anchor>,
    /// A virtual node (a loopback device or virtual camera), fed by another program. Only V4L2
    /// lists these.
    pub virtual_node: bool,
}

/// A backend could not list its cameras.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListError {
    pub backend: Backend,
    pub message: String,
}

impl ListError {
    pub fn new(backend: Backend, message: impl Into<String>) -> Self {
        Self {
            backend,
            message: message.into(),
        }
    }
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} listing failed: {}", self.backend, self.message)
    }
}

impl std::error::Error for ListError {}

/// What the native backend lists: Lemnos camera chains and USB cameras served by Lemnos.
pub trait NativeLister {
    fn list_native(&self) -> Result<Vec<Listed>, ListError>;
}

/// What libcamera lists. Returns no cameras, not an error, when libcamera is not available.
pub trait LibcameraLister {
    fn list_libcamera(&self) -> Result<Vec<Listed>, ListError>;
}

/// What V4L2 lists: every capture node, virtual ones marked.
pub trait V4l2Lister {
    fn list_v4l2(&self) -> Result<Vec<Listed>, ListError>;
}
