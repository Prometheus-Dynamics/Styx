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
//! A device that reports itself degraded but still answers is usable: it is served, and its
//! message is in the status. No current backend reports a degraded state through the `Capture`
//! trait, so the status shows the backend's retry messages only (`state=retrying`).
//!
//! # Frames
//!
//! Every frame of a camera is a `FrameLease` from the camera's own stream. The service's own
//! path does not copy or allocate per frame, and `styx_frame::copy_counters` stays zero (the
//! tests check both). The socket way out (a `styx-socket` producer per camera, so that other
//! processes read the frames without a copy) is not wired yet: its producer publishes only the
//! buffers it takes itself (`Producer::begin`), and the native streams fill the buffers Lemnos
//! hands out, so a camera's frames cannot be published without a copy until the stream can fill
//! buffers the producer supplies. Orion and Daedalus outlets are other crates' features and are
//! not wired here.
//!
//! # Not wired in this service
//!
//! - The 3A controller. The config accepts `tuning_dirs`, and nothing reads them yet.
//! - The socket and Orion and Daedalus ways out (see above). The [`Outlets`] trait is the hook.
//! - Degraded health from a backend (see above).

pub mod config;
pub mod control;
pub mod outlet;
pub mod service;
pub mod slot;
pub mod worker;

pub use config::{CameraConfig, Config, ConfigError, Force, GainLawName};
pub use outlet::{Discard, Outlet, Outlets};
pub use service::camera_socket;
pub use service::{Daemon, Deps, Error, Listers, Nothing, Service};
pub use slot::{CameraStatus, FrameSummary, Health, Publish, State};
