//! One stream of cameras appearing and leaving, from three sources.
//!
//! - Native cameras: Lemnos device events through a [`lemnos_client::Client`]. An event that a
//!   device left (`Off` or `BackingOff`) takes away every camera whose chain holds it, in arrival
//!   order; an event that a device is `Running` again re-lists the board. A device that goes off
//!   and comes back gives a departure, then an arrival.
//! - V4L2 nodes: the kernel's uevents on `NETLINK_KOBJECT_UEVENT` (through
//!   `lemnos_linux_sys::netlink`). An `add` or `remove` of a `video4linux` node is read from sysfs
//!   and the node is never opened. When the socket overflows, every node is re-listed from sysfs.
//! - libcamera: a one-time listing at start through [`styx_select::real::Libcamera`]. The shim
//!   exposes no hotplug signal, so libcamera arrivals and departures after start are not seen (see
//!   the PR text for what the shim needs). Nothing polls.
//!
//! There is no udev watcher of Styx's own and no thread: [`Watch::poll`] does the work on the
//! caller's thread. Identity is [`styx_select::Anchor`]: two sources' listings that share an anchor
//! are one [`Camera`], so one arrival and one departure, however many sources see it. A backend
//! never gives one camera twice, as in [`styx_select::choose`].
//!
//! Queues are fixed capacity. A source is read only while the queue has room, so a slow consumer
//! slows the kernel's and Lemnos's queues instead of growing ours.

mod backoff;
mod event;
mod native;
mod queue;
mod registry;
mod sysfs;
mod uevent;
mod v4l2;
mod watch;

pub use event::{Camera, Event, Kind, Member};
pub use sysfs::{VideoNode, scan_video};
pub use uevent::{Action, Uevent, parse_uevent};
pub use watch::{SourceStatus, Watch, WatchConfig, WatchStatus};
