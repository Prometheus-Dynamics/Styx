//! A sensor request as the controls a backend posts: exposure, analogue gain and frame rate,
//! each as a [`styx_controls::Update`] for the frame the request lands on.

use styx_aaa::SensorRequest;
use styx_controls::{Control, ControlValue, Update};

use crate::error::{ControllerError, Result};

/// The three controls of one sensor request, all for the same frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorUpdates {
    /// Exposure time, in microseconds.
    pub exposure: Update,
    /// Analogue gain, in thousandths (1000 is 1.0x).
    pub gain: Update,
    /// Frame rate, in millihertz (the reciprocal of the frame duration).
    pub frame_rate: Update,
}

impl SensorUpdates {
    /// The updates for `request`, checked against each control's range. A value outside it is
    /// an error, not a clamp: the algorithms asked for something no backend can post.
    pub fn from_request(request: &SensorRequest) -> Result<Self> {
        let frame = request.frame;
        let exposure_us = round_to_u32(request.exposure.as_secs_f64() * 1e6);
        let gain_milli = round_to_u32(request.analogue_gain * 1000.0);
        let rate_milli = round_to_u32(1000.0 / request.frame_duration.as_secs_f64());
        Ok(Self {
            exposure: update(Control::Exposure, exposure_us, frame)?,
            gain: update(Control::Gain, gain_milli, frame)?,
            frame_rate: update(Control::FrameRate, rate_milli, frame)?,
        })
    }

    /// The three updates, in [`Control`] order.
    pub fn iter(&self) -> impl Iterator<Item = Update> {
        [self.exposure, self.gain, self.frame_rate].into_iter()
    }
}

fn update(control: Control, value: u32, frame: u64) -> Result<Update> {
    let value = ControlValue::Manual(value);
    control
        .validate(value)
        .map_err(|error| ControllerError::Value { control, error })?;
    Ok(Update {
        control,
        value,
        frame,
    })
}

/// Rounds to the nearest whole number, saturating at the ends of `u32` (a negative or NaN value
/// gives 0; the range check then decides).
fn round_to_u32(v: f64) -> u32 {
    if v.is_nan() || v <= 0.0 {
        0
    } else {
        (v + 0.5) as u32
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use styx_controls::{Control, ControlValue, Update};

    use super::*;
    use crate::error::ControllerError;

    fn request(frame: u64, exposure: Duration, gain: f64, fd: Duration) -> SensorRequest {
        SensorRequest {
            frame,
            exposure,
            analogue_gain: gain,
            frame_duration: fd,
        }
    }

    #[test]
    fn a_request_is_three_updates_for_its_frame() {
        let fd = Duration::from_nanos(33_333_333);
        let u = SensorUpdates::from_request(&request(14, Duration::from_micros(10_400), 2.5, fd))
            .unwrap();
        let want = |control, value| Update {
            control,
            value: ControlValue::Manual(value),
            frame: 14,
        };
        assert_eq!(u.exposure, want(Control::Exposure, 10_400));
        assert_eq!(u.gain, want(Control::Gain, 2_500));
        assert_eq!(u.frame_rate, want(Control::FrameRate, 30_000));
        assert_eq!(u.iter().count(), 3);
    }

    #[test]
    fn a_value_outside_the_control_is_an_error_not_a_clamp() {
        let fd = Duration::from_millis(33);
        let too_long = request(3, Duration::from_secs(2), 1.0, fd);
        match SensorUpdates::from_request(&too_long) {
            Err(ControllerError::Value {
                control: Control::Exposure,
                ..
            }) => {}
            other => panic!("expected an exposure error, got {other:?}"),
        }
        let no_frame = request(3, Duration::from_millis(1), 1.0, Duration::ZERO);
        assert!(matches!(
            SensorUpdates::from_request(&no_frame),
            Err(ControllerError::Value {
                control: Control::FrameRate,
                ..
            })
        ));
    }
}
