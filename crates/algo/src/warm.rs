//! Warm starts: what the algorithms had settled on, to start the next session from.
//!
//! A camera restarted on the same scene (a new session, a mode switch such as 30 to 120 fps)
//! should not converge again from the tuning's start-up values. [`WarmStart`] carries AE's
//! total exposure (and the mode's sensitivity, so another mode can re-split it), the exposure
//! and gain that delivered it, and AWB's gains and temperature.
//! [`crate::Pipeline::warm_state`] takes one from the latest parameters and
//! [`crate::Pipeline::prepare_warm`] starts from one. It is plain data (serde), so callers can
//! keep it per camera in memory or on disk.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::params::Params;

/// Settled algorithm state to start from. See the [module documentation](self).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WarmStart {
    /// AE's total exposure: seconds × analogue gain × digital gain.
    pub total_exposure: f64,
    /// Sensitivity of the mode it was found in (`CameraConfig::sensitivity`).
    pub sensitivity: f64,
    /// The exposure time that delivered it.
    pub exposure: Duration,
    /// The analogue gain that delivered it.
    pub analogue_gain: f64,
    /// AE was locked.
    pub ae_locked: bool,
    /// White balance gains (r, g, b).
    pub colour_gains: [f64; 3],
    /// Colour temperature (kelvin).
    pub colour_temperature: f64,
    /// AWB had converged.
    pub awb_converged: bool,
    /// Scene illuminance estimate (lux).
    pub lux: f64,
    /// The light flicker period AE detected (`Flicker::Auto`): the next session avoids it
    /// from its first frame.
    pub flicker_detected: Option<Duration>,
}

impl Default for WarmStart {
    fn default() -> Self {
        Self {
            total_exposure: 0.0,
            sensitivity: 1.0,
            exposure: Duration::ZERO,
            analogue_gain: 1.0,
            ae_locked: false,
            colour_gains: [1.0; 3],
            colour_temperature: 4500.0,
            awb_converged: false,
            lux: 400.0,
            flicker_detected: None,
        }
    }
}

impl WarmStart {
    /// From the parameters of the latest frame in a mode of `sensitivity`. `None` before AE
    /// produced anything.
    pub fn from_params(p: &Params, sensitivity: f64) -> Option<Self> {
        let s = p.sensor?;
        let total = if p.ae.total_exposure > 0.0 {
            p.ae.total_exposure
        } else {
            s.exposure.as_secs_f64() * s.analogue_gain * p.digital_gain.max(1.0)
        };
        Some(Self {
            total_exposure: total,
            sensitivity,
            exposure: s.exposure,
            analogue_gain: s.analogue_gain,
            ae_locked: p.ae.locked,
            colour_gains: p.colour_gains,
            colour_temperature: p.colour_temperature,
            awb_converged: p.awb.converged,
            lux: p.lux,
            flicker_detected: p.ae.flicker_detected,
        })
    }

    /// Whether the values can be used (positive and finite).
    pub fn is_valid(&self) -> bool {
        let pos = |v: f64| v.is_finite() && v > 0.0;
        pos(self.total_exposure)
            && pos(self.sensitivity)
            && pos(self.analogue_gain)
            && !self.exposure.is_zero()
            && self.colour_gains.iter().all(|g| pos(*g))
            && pos(self.colour_temperature)
    }
}
