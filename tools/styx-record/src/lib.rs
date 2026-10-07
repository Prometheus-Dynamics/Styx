//! `styx-record`: grey test recordings (the Y plane at native resolution) in Eidos's raw grey
//! video layout, with a per-frame timestamp sidecar and the camera settings, from a Styx camera
//! service (a normal client next to the others) or from a camera opened directly. See
//! `docs/recording.md`.

#![cfg(target_os = "linux")]

pub mod config;
pub mod json;
pub mod luma;
pub mod record;
pub mod settings;
pub mod sha256;
pub mod sidecar;
pub mod signal;
pub mod source;
pub mod writer;

pub use config::{Config, Mode, SourceKind, StillTrigger, Stills};
pub use record::{Summary, record, run};
