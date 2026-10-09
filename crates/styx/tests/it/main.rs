//! `styx`'s integration tests in one binary: one module per former `tests/*.rs` file, so an
//! edit relinks one test binary instead of 14 (docs/development.md#the-gate).
//!
//! Modules that need features are compiled in when they are on (formerly `required-features`).
//! Binaries of their own: `zero_alloc` (a counting `#[global_allocator]`) and `metrics` (it
//! checks the process-wide count of camera service clients, which another module's service
//! would change while `cargo test` runs this binary's tests in threads).

mod support;

#[cfg(all(
    target_os = "linux",
    feature = "codec-turbojpeg",
    feature = "replay-mcap"
))]
mod camera_service;
#[cfg(target_os = "linux")]
mod connection_events;
#[cfg(target_os = "linux")]
mod control_client;
mod docker_facade_examples;
#[cfg(all(target_os = "linux", feature = "codec-ffmpeg"))]
mod encoded_frames;
#[cfg(target_os = "linux")]
mod frame_client_async;
#[cfg(feature = "facade")]
mod frame_delivery;
#[cfg(feature = "facade")]
mod lazy_decode;
#[cfg(target_os = "linux")]
mod multicam;
#[cfg(all(target_os = "linux", feature = "preview"))]
mod preview;
#[cfg(feature = "facade")]
mod regions;
#[cfg(all(feature = "codec-turbojpeg", feature = "replay-mcap"))]
mod scaled_planning;
#[cfg(target_os = "linux")]
mod service_controls;
#[cfg(all(feature = "codec-turbojpeg", feature = "replay-mcap"))]
mod shared_planning;
