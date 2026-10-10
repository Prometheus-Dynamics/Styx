//! Predicted landing frames for a processed mode's fixed exposure and gain.

use styx_native::styx_sensor::Control as SensorControl;

use super::{CameraControls, ControlId, ControlValue, controls};

/// The sensor controls a processed mode's fixed value is written to: exposure, or the analogue
/// and digital gains (a total gain is written analogue first). `None` for every other control
/// and for a value of 0, which hands the control back to AE: no frame is then fixed.
pub(crate) fn fixed_sensor_controls(
    id: ControlId,
    value: &ControlValue,
) -> Option<&'static [SensorControl]> {
    let positive = match value {
        ControlValue::Uint(v) => *v > 0,
        ControlValue::Int(v) => *v > 0,
        ControlValue::Float(v) => *v > 0.0,
        _ => false,
    };
    if !positive {
        return None;
    }
    match id {
        controls::EXPOSURE_TIME_US => Some(&[SensorControl::Exposure]),
        controls::GAIN => Some(&[SensorControl::AnalogGain, SensorControl::DigitalGain]),
        _ => None,
    }
}

/// The first frame a processed mode's fixed exposure or gain takes effect on: the 3A loop writes
/// it during the frame in progress, so the sensor's landing for that write (current frame plus
/// the control's delay, see [`CameraControls::landing_now`]). A predicted landing: the loop's
/// write can miss the frame and land one later, and a sensor with embedded data reports the
/// frame's values with `NativeFrameMeta::verified`. `None` where no frame is fixed (AE, EV, AWB).
pub(crate) fn processed_landing(
    controls: &CameraControls,
    id: ControlId,
    value: &ControlValue,
) -> Option<u64> {
    fixed_sensor_controls(id, value).map(|cs| {
        cs.iter()
            .map(|c| controls.landing_now(*c))
            .max()
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fixed_exposure_or_gain_fixes_a_frame() {
        let fixed = |id, v| fixed_sensor_controls(id, &v);
        assert_eq!(
            fixed(controls::EXPOSURE_TIME_US, ControlValue::Uint(5000)),
            Some(&[SensorControl::Exposure][..])
        );
        assert_eq!(
            fixed(controls::GAIN, ControlValue::Float(2.0)),
            Some(&[SensorControl::AnalogGain, SensorControl::DigitalGain][..])
        );
        // 0 hands the control back to AE: no frame is fixed.
        assert_eq!(
            fixed(controls::EXPOSURE_TIME_US, ControlValue::Uint(0)),
            None
        );
        assert_eq!(fixed(controls::GAIN, ControlValue::Float(0.0)), None);
        // AE, EV, AWB and colour temperature fix no frame.
        assert_eq!(fixed(controls::AE_ENABLE, ControlValue::Bool(false)), None);
        assert_eq!(
            fixed(controls::EXPOSURE_VALUE, ControlValue::Float(1.0)),
            None
        );
        assert_eq!(fixed(controls::AWB_ENABLE, ControlValue::Bool(false)), None);
        assert_eq!(
            fixed(controls::COLOUR_TEMPERATURE, ControlValue::Uint(5000)),
            None
        );
    }
}
