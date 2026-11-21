//! The build without libcamera: every entry point reports it unavailable.

use crate::{CaptureStats, Error};
use styx_controls::Update;

/// Why the backend is unavailable in this build.
pub(crate) const REASON: &str = "built without the libcamera feature";

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

    /// Every control the camera offers: none in this build.
    pub fn device_controls(&self) -> Vec<crate::DeviceEntry> {
        Vec::new()
    }

    /// The Styx controls the camera can be set through: none in this build.
    pub fn standard_controls(&self) -> Vec<crate::StandardControl> {
        Vec::new()
    }

    pub fn controls_generation(&self) -> u64 {
        0
    }

    pub fn get_device_control(&self, _id: u32) -> Result<i64, Error> {
        Err(Error::Unavailable(REASON))
    }

    pub fn set_device_control(&self, _id: u32, _value: i64) -> Result<(), Error> {
        Err(Error::Unavailable(REASON))
    }
}
