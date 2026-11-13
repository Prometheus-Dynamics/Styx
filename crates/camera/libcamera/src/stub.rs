//! The build without libcamera: every entry point reports it unavailable.

use crate::{CaptureStats, Error};
use styx_controls::Update;

/// Why the backend is unavailable in this build.
pub(crate) const REASON: &str = if cfg!(feature = "libcamera") {
    "libcamera was not found by pkg-config"
} else {
    "built without the libcamera feature"
};

/// A capture, which never exists in this build.
#[derive(Debug)]
pub struct Capture(());

impl Capture {
    pub fn set_control(&self, _update: Update) -> Result<(), Error> {
        Err(Error::Unavailable(REASON))
    }

    pub fn stats(&self) -> CaptureStats {
        CaptureStats::default()
    }

    pub fn camera_id(&self) -> String {
        String::new()
    }
}
