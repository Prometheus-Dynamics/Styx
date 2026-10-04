//! Sensor descriptions compiled at build time: validated on the host from TOML and stored in
//! a compact binary form (postcard), so a target without `std` needs no TOML parser and a
//! broken description fails the firmware build instead of the boot.
//!
//! In the firmware's `build.rs` (`styx-sensor` with feature `build` as a build-dependency):
//!
//! ```no_run
//! # #[cfg(feature = "build")] {
//! // build.rs
//! styx_sensor::build::compile(&["sensors/ov5640.toml"]).unwrap();
//! styx_sensor::build::compile_builtin(&["ov9782"]).unwrap();
//! # }
//! ```
//!
//! and in the firmware (`styx-sensor` with feature `postcard`, no `std`):
//!
//! ```ignore
//! let desc = SensorDescription::from_postcard(styx_sensor::include_description!("ov5640"))?;
//! ```

use alloc::borrow::ToOwned;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::desc::SensorDescription;
use crate::error::{Result, SensorError};

impl SensorDescription {
    /// A description from its compiled form ([`Self::to_postcard`], `build::compile`), checked
    /// again with [`Self::validate`].
    pub fn from_postcard(bytes: &[u8]) -> Result<Self> {
        let desc: Self = postcard::from_bytes(bytes).map_err(|e| SensorError::Parse {
            source_name: "compiled description".to_owned(),
            message: e.to_string(),
        })?;
        desc.validate().map_err(|issues| SensorError::Invalid {
            source_name: desc.sensor.name.clone(),
            issues,
        })?;
        Ok(desc)
    }

    /// The compiled form.
    pub fn to_postcard(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("descriptions always serialize")
    }
}

/// The bytes of a description compiled by `styx_sensor::build` in this crate's build script,
/// by sensor name, for [`SensorDescription::from_postcard`].
#[macro_export]
macro_rules! include_description {
    ($name:literal) => {
        include_bytes!(concat!(
            env!("OUT_DIR"),
            "/styx-sensor/",
            $name,
            ".postcard"
        ))
    };
}

#[cfg(test)]
mod tests {
    use crate::{BUILTIN_DESCRIPTIONS, BUILTIN_KERNEL_DATA, SensorDescription};

    #[test]
    fn every_builtin_description_round_trips() {
        for (name, toml) in BUILTIN_DESCRIPTIONS {
            let d = SensorDescription::from_toml_str(toml, name).unwrap();
            let bytes = d.to_postcard();
            assert!(
                bytes.len() < toml.len() / 2,
                "{name}: {} bytes",
                bytes.len()
            );
            assert_eq!(SensorDescription::from_postcard(&bytes).unwrap(), d);
        }
        assert!(!BUILTIN_KERNEL_DATA.is_empty());
        assert!(SensorDescription::from_postcard(&[1, 2, 3]).is_err());
    }
}
