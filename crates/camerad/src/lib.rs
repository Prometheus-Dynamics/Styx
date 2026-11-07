//! Styx's camera daemon: a [`styx::ipc::CameraService`] for each attached camera, started when
//! the camera appears and stopped when it goes, each on its own Unix socket named after the
//! camera's identity key, and the list of those endpoints published whenever it changes.
//!
//! A product embeds it ([`run`], or [`Daemon`] to drive it itself) and turns the endpoint list
//! into whatever it advertises; the `styx-camerad` binary runs it with a JSON configuration
//! file and the list as a JSON file. Per camera: its capture settings (ISP output buffers,
//! slices, queues), its clients' limit, and the CPUs its threads run on (so capture stays off a
//! vision core).
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use styx_camerad::{Config, Endpoint};
//!
//! let config = Config {
//!     cpus: Some(vec![0, 1]),
//!     ..Config::default()
//! };
//! let stop = AtomicBool::new(false);
//! styx_camerad::run(config, |endpoints: &[Endpoint]| {
//!     for e in endpoints {
//!         println!("{} at {}", e.name, e.socket.display());
//!     }
//! }, &stop)
//! .unwrap();
//! ```

pub mod affinity;
mod config;
mod daemon;
pub mod endpoints;

use std::sync::atomic::AtomicBool;

pub use config::{CameraConfig, Config, Resolved};
pub use daemon::{Daemon, StartError, socket_path, socket_stem};
pub use endpoints::{Endpoint, EndpointSink, JsonFile, TRANSPORT};

/// Serve cameras as `config` says until `stop` is set, publishing their endpoints to `sink`
/// (and to [`Config::endpoints_file`], when set).
pub fn run(
    config: Config,
    sink: impl EndpointSink,
    stop: &AtomicBool,
) -> Result<(), styx::watch::WatchError> {
    match config.endpoints_file.clone() {
        Some(file) => Daemon::new(config, endpoints::Both(sink, JsonFile::new(file))).run(stop),
        None => Daemon::new(config, sink).run(stop),
    }
}

#[cfg(test)]
mod tests;
