//! The gain law: how a gain in thousandths becomes the sensor's code, and back.
//!
//! `camera.sensor/1` at the lock gives a gain range's codes (`min`, `max`, `unity`, `step`) and
//! says the unity code is the code for a gain of one. It does not say how the codes relate to the
//! gain in between: no law is in the contract at the lock. So the law is a setting the caller
//! gives, never assumed. [`GainLaw::Unknown`] (the default) is refused: no gain code is produced
//! for it, and [`NativeControls::new`](crate::NativeControls::new) fails with [`Error::GainLaw`].
//!
//! Every gain conversion goes through [`gain_to_code`] and [`code_to_gain`], so a later contract
//! field (Lemnos J4 adds the law to the contract) drops into these two functions and nowhere else.

use lemnos_contract_camera_sensor::GainRange;

use crate::error::Error;
use crate::units;

/// The relation between a sensor's gain codes and the gain they give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum GainLaw {
    /// Not known to the caller. Refused: no gain code is produced for it.
    #[default]
    Unknown,
    /// Linear in unity: `code = unity * gain`, moved in the range's `step`s from its `min`.
    Linear,
}

/// The sensor's gain code for `milli` thousandths of gain, under `law`.
///
/// Refused with [`Error::GainLaw`] for [`GainLaw::Unknown`], and with [`Error::Mode`] when the
/// range has no unity code. Does not check the range's bounds: the caller does.
pub fn gain_to_code(law: GainLaw, range: &GainRange, milli: u32) -> Result<u32, Error> {
    match law {
        GainLaw::Unknown => Err(Error::GainLaw),
        GainLaw::Linear => units::gain_code(milli, range)
            .and_then(|code| u32::try_from(code).ok())
            .ok_or(Error::Mode("analogue gain has no unity code")),
    }
}

/// Thousandths of gain for a sensor's gain code, under `law`. The inverse of [`gain_to_code`].
pub fn code_to_gain(law: GainLaw, range: &GainRange, code: u32) -> Result<u32, Error> {
    match law {
        GainLaw::Unknown => Err(Error::GainLaw),
        GainLaw::Linear if range.unity == 0 => Err(Error::Mode("analogue gain has no unity code")),
        GainLaw::Linear => Ok(units::gain_milli(u64::from(code), range.unity)),
    }
}
