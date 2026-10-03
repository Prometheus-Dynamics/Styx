//! Controls of a processed native capture's 3A loop: what the application asks of AE and AWB
//! (on or off, a fixed exposure or gain, exposure compensation, a manual white balance), and
//! what the loop reports back (AE state, colour temperature). Shared between the capture's
//! control plane and its worker, which hands a changed set to the loop before its next frame.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use styx_capture::prelude::{ControlId, ControlValue};
use styx_pipeline::styx_algo::{Controls, Params};

use super::super::native_backend::controls as ids;
use super::super::request::CaptureError;

/// The loop's application controls and its latest state.
#[derive(Debug)]
pub struct LoopControls {
    current: Mutex<Controls>,
    pending: Mutex<Option<Controls>>,
    /// libcamera's `AeState`: 1 searching, 2 converged (locked).
    ae_state: AtomicI32,
    /// AWB's colour temperature for the latest frame, in kelvin.
    colour_temperature: AtomicU32,
}

impl Default for LoopControls {
    fn default() -> Self {
        Self {
            current: Mutex::new(Controls::default()),
            pending: Mutex::new(None),
            ae_state: AtomicI32::new(1),
            colour_temperature: AtomicU32::new(0),
        }
    }
}

fn number(value: &ControlValue) -> Result<f64, CaptureError> {
    Ok(match value {
        ControlValue::Bool(v) => f64::from(u8::from(*v)),
        ControlValue::Uint(v) => f64::from(*v),
        ControlValue::Int(v) => f64::from(*v),
        ControlValue::Float(v) => f64::from(*v),
        _ => return Err(CaptureError::control_apply("native controls take numbers")),
    })
}

impl LoopControls {
    /// Applies a control to the loop. `None` when `id` is not one of the loop's.
    ///
    /// Exposure time and gain fix that value (AE then moves only the other one; both fixed is
    /// manual exposure); 0 hands it back to AE, as libcamera's Raspberry Pi IPA does.
    pub(crate) fn apply(
        &self,
        id: ControlId,
        value: &ControlValue,
    ) -> Option<Result<(), CaptureError>> {
        const LOOP: [ControlId; 10] = [
            ids::EXPOSURE_TIME_US,
            ids::GAIN,
            ids::AE_ENABLE,
            ids::EXPOSURE_VALUE,
            ids::AWB_ENABLE,
            ids::COLOUR_TEMPERATURE,
            ids::RED_GAIN,
            ids::BLUE_GAIN,
            ids::FRAME_RATE,
            ids::FRAME_DURATION_US,
        ];
        if !LOOP.contains(&id) {
            return None;
        }
        let v = match number(value) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let positive = (v > 0.0).then_some(v);
        let mut c = self.current.lock();
        match id {
            ids::EXPOSURE_TIME_US => {
                c.exposure = positive.map(|us| Duration::from_secs_f64(us / 1e6))
            }
            ids::GAIN => c.analogue_gain = positive,
            ids::AE_ENABLE => c.ae_enable = v != 0.0,
            ids::EXPOSURE_VALUE => c.ev = v,
            ids::AWB_ENABLE => c.awb_enable = v != 0.0,
            ids::COLOUR_TEMPERATURE => {
                c.colour_temperature = positive;
                c.colour_gains = None;
            }
            ids::RED_GAIN | ids::BLUE_GAIN => {
                let (mut r, mut b) = c.colour_gains.unwrap_or((1.0, 1.0));
                if id == ids::RED_GAIN {
                    r = v;
                } else {
                    b = v;
                }
                c.colour_gains = positive.map(|_| (r, b));
            }
            ids::FRAME_RATE | ids::FRAME_DURATION_US => {
                return Some(Err(CaptureError::control_apply(
                    "a processed native capture keeps the frame rate it started with (AE \
                     chooses exposures within it); start it again at another rate \
                     (CaptureHandle::reconfigure)",
                )));
            }
            _ => unreachable!("checked above"),
        }
        *self.pending.lock() = Some(c.clone());
        Some(Ok(()))
    }

    /// Reads one of the loop's controls (`None` for the camera's own).
    pub(crate) fn read(&self, id: ControlId) -> Option<ControlValue> {
        let c = self.current.lock();
        Some(match id {
            ids::AE_STATE => ControlValue::Int(self.ae_state.load(Ordering::Acquire)),
            ids::AE_ENABLE => ControlValue::Bool(c.ae_enable),
            ids::EXPOSURE_VALUE => ControlValue::Float(c.ev as f32),
            ids::AWB_ENABLE => ControlValue::Bool(c.awb_enable),
            ids::COLOUR_TEMPERATURE => {
                ControlValue::Uint(self.colour_temperature.load(Ordering::Acquire))
            }
            ids::RED_GAIN => ControlValue::Float(c.colour_gains.map_or(0.0, |g| g.0) as f32),
            ids::BLUE_GAIN => ControlValue::Float(c.colour_gains.map_or(0.0, |g| g.1) as f32),
            _ => return None,
        })
    }

    /// Controls changed since the last call, for the loop.
    pub(crate) fn take(&self) -> Option<Controls> {
        self.pending.lock().take()
    }

    /// Records what the loop made of a frame.
    pub(crate) fn report(&self, params: &Params) {
        self.ae_state
            .store(if params.ae.locked { 2 } else { 1 }, Ordering::Release);
        self.colour_temperature
            .store(params.colour_temperature.round() as u32, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_and_white_balance_controls_reach_the_loop() {
        let l = LoopControls::default();
        assert!(l.take().is_none());
        l.apply(ids::EXPOSURE_TIME_US, &ControlValue::Uint(8000))
            .unwrap()
            .unwrap();
        l.apply(ids::AWB_ENABLE, &ControlValue::Bool(false))
            .unwrap()
            .unwrap();
        l.apply(ids::COLOUR_TEMPERATURE, &ControlValue::Uint(5000))
            .unwrap()
            .unwrap();
        let c = l.take().unwrap();
        assert_eq!(c.exposure, Some(Duration::from_millis(8)));
        assert!(c.ae_enable && !c.awb_enable);
        assert_eq!(c.colour_temperature, Some(5000.0));
        assert!(l.take().is_none());
        // 0 hands the exposure back to AE; gains replace the temperature.
        l.apply(ids::EXPOSURE_TIME_US, &ControlValue::Uint(0))
            .unwrap()
            .unwrap();
        l.apply(ids::RED_GAIN, &ControlValue::Float(1.5))
            .unwrap()
            .unwrap();
        let c = l.take().unwrap();
        assert_eq!(c.exposure, None);
        assert_eq!(c.colour_gains, Some((1.5, 1.0)));
        assert_eq!(l.read(ids::RED_GAIN), Some(ControlValue::Float(1.5)));
        assert_eq!(l.read(ids::AE_STATE), Some(ControlValue::Int(1)));
        assert!(
            l.apply(ids::FRAME_RATE, &ControlValue::Float(60.0))
                .unwrap()
                .is_err()
        );
        assert!(l.apply(ControlId(1), &ControlValue::Uint(1)).is_none());
    }
}
