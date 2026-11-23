//! The camera service: one process per board, holding every camera.
//!
//! [`Service`] lists the cameras of each backend through `styx-select` (the choice of owner is
//! the chooser's, and nothing here opens a device through a backend directly). Each camera runs
//! on its own thread ([`worker`]) and is served for as long as the service runs. A control socket
//! ([`control`]) answers the camera list, the controls, and a manual start and stop. Frames go to
//! the ways out: an [`Outlet`] per camera, called on the camera's thread with each frame's lease
//! ([`outlet`]).
//!
//! The binary `styx-service` reads a TOML config (see [`config`] for every key) and runs a
//! [`Service`] until it is stopped. A systemd unit is in `systemd/styx-service.service`: it has
//! no start limits, restarts always, and waits a short time between restarts.
//!
//! # Failure rule
//!
//! A camera that leaves or fails comes back: its worker waits and opens it again, for as long as
//! the service runs. The waits double from `backoff_first_ms` and hold at `backoff_cap_ms`, which
//! must be 5 to 10 seconds. A camera that delivered a frame since its last attempt starts again at
//! the first wait. The backoff state is in the status (`state=backoff`, `next_delay_ms`,
//! `last_error`). Nothing turns a camera off by itself unless the config sets `auto_off_after`.
//! A manual `stop` ends a camera until a manual `start`.
//!
//! # Shutdown and a sensor left started
//!
//! SIGTERM and SIGINT end the process through [`Service::shutdown`] (see `main.rs`): every camera's
//! thread ends, and its session stops the receiver and then the sensor through the Lemnos contract
//! before the thread does. Receiver first is deliberate (styx-native-run's `halt` says why, with
//! the board result): sensor first wedged a board's receiver until a reboot. The sockets are
//! removed last. A SIGKILL cannot be caught, so a sensor can be left streaming. The next start
//! then gets a busy refusal from the sensor's mode selection: that camera's worker stops the receiver and sensor once through the contract and tries again
//! once, in the same attempt. The stop is counted in the status (`recoveries`, `last_recovery`). A
//! second refusal is an ordinary failure and backs off like any other.
//!
//! A device that reports itself degraded but still answers is usable: it is served, and its
//! message is in the status. No current backend reports a degraded state through the `Capture`
//! trait, so the status shows the backend's retry messages only (`state=retrying`).
//!
//! # Frames
//!
//! Every frame of a camera is a `FrameLease` from the camera's own stream. The service's raw path
//! does not copy or allocate per frame, and `styx_frame::copy_counters` stays zero for it (the
//! tests check both). Each camera's frames are published on its own socket by a `styx-socket`
//! producer over the camera's own pool (`Producer::with_pool`), so other processes read the
//! buffers the camera filled. With the config's `isp` key set, each processed output has a socket
//! of its own; the PiSP back end's input is one copy of the raw frame, counted at the copy site
//! `pisp-be-input`. Orion and Daedalus readers are other crates' features (`styx-orion`,
//! `styx-daedalus`) and read these sockets; they are not wired into the service.
//!
//! # 3A
//!
//! With `aaa = true` beside `isp`, a native camera's processed stream runs the 3A loop (`aaa`):
//! auto exposure, auto white balance and flicker avoidance from the tuning the search finds in
//! `tuning_dirs` (then `STYX_TUNING_PATH`, `/etc/styx/tuning` and the bundled tunings), each part on, off or manual at run time through the
//! control socket. Without it, exposure and gain stay as set and the ISP runs neutral settings.
//!
//! # Not wired in this service
//!
//! - Orion and Daedalus ways out inside the service (see above). The [`Outlets`] trait is the hook.
//! - Degraded health from a backend (see above).

pub mod aaa;
pub mod access;
pub mod config;
pub mod control;
mod isp;
pub mod isp_switch;
pub mod outlet;
pub mod service;
pub mod slot;
pub mod worker;

pub use aaa::AaaStatus;
pub use access::Access;
pub use config::{
    CameraConfig, Config, ConfigError, Force, GroupSpec, IspMode, IspOutputSpec, IspPlan,
    StatsSampling,
};
pub use outlet::{Discard, Outlet, Outlets};
pub use service::{Daemon, Deps, Error, Listers, Nothing, Service};
pub use service::{camera_socket, isp_socket};
pub use slot::{CameraStatus, FrameSummary, Health, IspStatus, Publish, State};
