//! Styx's controls on a node's V4L2 controls. Each mapped control has one conversion, both ways.
//!
//! Exposure is in microseconds here and in 100 us units on the wire (the V4L2 unit of
//! `V4L2_CID_EXPOSURE_ABSOLUTE`). Focus is normalised 0 to 1000 here and in the driver's range
//! there. Frame rate has no V4L2 control: it is the frame interval. Gain and crop are not mapped
//! yet: gain's unit is the driver's, and crop is the selection API, not a control.

use styx_controls::{Control, ControlValue};
use styx_lemnos::v4l2::Fraction;
use styx_lemnos::v4l2::v4l2::{
    BufType, ControlInfo, ControlType, ControlValue as Raw, Controls, VideoDevice, cid,
};

use crate::Error;

/// A driver control's integer range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) id: u32,
    pub(crate) min: i64,
    pub(crate) max: i64,
}

/// The exposure mode menu: `manual` turns automatic exposure off, `auto` turns it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExposureMode {
    id: u32,
    manual: i32,
    auto: i32,
}

/// The controls one node maps, as they were probed. `Copy`, so a stream keeps its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Table {
    exposure: Option<Span>,
    exposure_mode: Option<ExposureMode>,
    white_balance: Option<Span>,
    white_balance_auto: Option<u32>,
    focus: Option<Span>,
    focus_auto: Option<u32>,
    h_flip: Option<u32>,
    v_flip: Option<u32>,
    frame_rate: bool,
}

impl Table {
    /// The mapped controls of `dev`, with the queue `buf_type` for the frame rate.
    pub(crate) fn build(dev: &VideoDevice, buf_type: BufType) -> Table {
        // A node without control support answers with no controls; that is an empty table.
        let infos = dev.query_controls().unwrap_or_default();
        let find = |id: u32| infos.iter().find(|info| info.id == id);
        let integer = |id: u32| {
            find(id)
                .filter(|info| matches!(info.control_type, ControlType::Integer))
                .map(|info| Span {
                    id,
                    min: info.minimum,
                    max: info.maximum,
                })
        };
        let flag = |id: u32| {
            find(id)
                .filter(|info| matches!(info.control_type, ControlType::Boolean))
                .map(|info| info.id)
        };
        Table {
            exposure: integer(cid::EXPOSURE_ABSOLUTE),
            exposure_mode: find(cid::EXPOSURE_AUTO).and_then(|info| exposure_mode(dev, info)),
            white_balance: integer(cid::WHITE_BALANCE_TEMPERATURE),
            white_balance_auto: flag(cid::AUTO_WHITE_BALANCE),
            focus: integer(cid::FOCUS_ABSOLUTE),
            focus_auto: flag(cid::FOCUS_AUTO),
            h_flip: flag(cid::HFLIP),
            v_flip: flag(cid::VFLIP),
            frame_rate: dev.stream_params(buf_type).is_ok(),
        }
    }

    /// Whether this node maps `control`.
    pub(crate) fn supports(&self, control: Control) -> bool {
        match control {
            Control::Exposure => self.exposure.is_some(),
            Control::WhiteBalance => self.white_balance.is_some(),
            Control::Focus => self.focus.is_some(),
            Control::HFlip => self.h_flip.is_some(),
            Control::VFlip => self.v_flip.is_some(),
            Control::FrameRate => self.frame_rate,
            Control::Gain | Control::Crop => false,
        }
    }

    /// The controls this node maps, in [`Control::ALL`] order.
    pub(crate) fn supported(&self) -> Vec<Control> {
        Control::ALL
            .into_iter()
            .filter(|&control| self.supports(control))
            .collect()
    }

    /// Sets `control` to `value` on the device. The value must be one the control takes.
    pub(crate) fn apply(
        &self,
        dev: &VideoDevice,
        buf_type: BufType,
        control: Control,
        value: ControlValue,
    ) -> Result<(), Error> {
        if !self.supports(control) {
            return Err(Error::NoControl(control));
        }
        control.validate(value)?;
        match (control, value) {
            (Control::Exposure, ControlValue::Auto) => {
                let mode = self.exposure_mode.ok_or_else(|| {
                    Error::Unsupported("exposure has no automatic mode on this node".into())
                })?;
                dev.set_control(mode.id, Raw::Integer(mode.auto))?;
            }
            (Control::Exposure, ControlValue::Manual(us)) => {
                let span = span(self.exposure, control)?;
                if let Some(mode) = self.exposure_mode {
                    dev.set_control(mode.id, Raw::Integer(mode.manual))?;
                }
                let raw = exposure_to_raw(us, span);
                dev.set_control(span.id, Raw::Integer(to_i32(raw)))?;
            }
            (Control::WhiteBalance, ControlValue::Auto) => {
                let id = self.white_balance_auto.ok_or_else(|| {
                    Error::Unsupported("white balance has no automatic mode on this node".into())
                })?;
                dev.set_control(id, Raw::Integer(1))?;
            }
            (Control::WhiteBalance, ControlValue::Manual(kelvin)) => {
                let span = span(self.white_balance, control)?;
                if let Some(id) = self.white_balance_auto {
                    dev.set_control(id, Raw::Integer(0))?;
                }
                let raw = i64::from(kelvin).clamp(span.min, span.max);
                dev.set_control(span.id, Raw::Integer(to_i32(raw)))?;
            }
            (Control::Focus, ControlValue::Auto) => {
                let id = self.focus_auto.ok_or_else(|| {
                    Error::Unsupported("focus has no automatic mode on this node".into())
                })?;
                dev.set_control(id, Raw::Integer(1))?;
            }
            (Control::Focus, ControlValue::Manual(position)) => {
                let span = span(self.focus, control)?;
                if let Some(id) = self.focus_auto {
                    dev.set_control(id, Raw::Integer(0))?;
                }
                let raw = focus_to_raw(position, span);
                dev.set_control(span.id, Raw::Integer(to_i32(raw)))?;
            }
            (Control::HFlip, ControlValue::Flag(on)) => {
                let id = self.h_flip.ok_or(Error::NoControl(control))?;
                dev.set_control(id, Raw::Integer(i32::from(on)))?;
            }
            (Control::VFlip, ControlValue::Flag(on)) => {
                let id = self.v_flip.ok_or(Error::NoControl(control))?;
                dev.set_control(id, Raw::Integer(i32::from(on)))?;
            }
            (Control::FrameRate, ControlValue::Manual(millihertz)) => {
                // Seconds per frame is 1000 / millihertz, validated to be at least 1000 mHz.
                dev.set_frame_interval(buf_type, Fraction::new(1000, millihertz))?;
            }
            _ => return Err(Error::NoControl(control)),
        }
        Ok(())
    }

    /// What `control` is set to on the device, when this node can say.
    pub(crate) fn read(
        &self,
        dev: &VideoDevice,
        buf_type: BufType,
        control: Control,
    ) -> Option<ControlValue> {
        if !self.supports(control) {
            return None;
        }
        match control {
            Control::Exposure => {
                let span = self.exposure?;
                if let Some(mode) = self.exposure_mode
                    && read_int(dev, mode.id)? != i64::from(mode.manual)
                {
                    return Some(ControlValue::Auto);
                }
                Some(ControlValue::Manual(raw_to_exposure(read_int(
                    dev, span.id,
                )?)))
            }
            Control::WhiteBalance => {
                let span = self.white_balance?;
                if let Some(id) = self.white_balance_auto
                    && read_int(dev, id)? != 0
                {
                    return Some(ControlValue::Auto);
                }
                let kelvin = read_int(dev, span.id)?.clamp(0, i64::from(u32::MAX));
                Some(ControlValue::Manual(u32::try_from(kelvin).ok()?))
            }
            Control::Focus => {
                let span = self.focus?;
                if let Some(id) = self.focus_auto
                    && read_int(dev, id)? != 0
                {
                    return Some(ControlValue::Auto);
                }
                Some(ControlValue::Manual(raw_to_focus(
                    read_int(dev, span.id)?,
                    span,
                )))
            }
            Control::HFlip => Some(ControlValue::Flag(read_int(dev, self.h_flip?)? != 0)),
            Control::VFlip => Some(ControlValue::Flag(read_int(dev, self.v_flip?)? != 0)),
            Control::FrameRate => {
                let params = dev.stream_params(buf_type).ok()?;
                let seconds = params.time_per_frame.as_f64();
                if !seconds.is_finite() || seconds <= 0.0 {
                    return None;
                }
                let millihertz = (1.0 / seconds * 1000.0).round();
                Some(ControlValue::Manual(millihertz as u32))
            }
            Control::Gain | Control::Crop => None,
        }
    }
}

/// The exposure mode menu, when the node has one with a manual entry and an automatic one.
/// Aperture priority (3) is automatic exposure on UVC; shutter priority (2) is the fallback.
fn exposure_mode(dev: &VideoDevice, info: &ControlInfo) -> Option<ExposureMode> {
    if !info.is_menu() {
        return None;
    }
    let items = dev.query_menu(info).ok()?;
    let has = |index: u32| items.iter().any(|item| item.index == index);
    if !has(1) {
        return None;
    }
    let auto = if has(3) {
        3
    } else if has(2) {
        2
    } else {
        return None;
    };
    Some(ExposureMode {
        id: cid::EXPOSURE_AUTO,
        manual: 1,
        auto,
    })
}

fn span(span: Option<Span>, control: Control) -> Result<Span, Error> {
    span.ok_or(Error::NoControl(control))
}

fn read_int(dev: &VideoDevice, id: u32) -> Option<i64> {
    dev.control(id).ok()?.as_i64()
}

fn to_i32(value: i64) -> i32 {
    // Every value here was clamped to a driver range, which V4L2 keeps within `i32`.
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Microseconds to the driver's 100 us units, rounded up, within the driver's range.
pub(crate) fn exposure_to_raw(microseconds: u32, span: Span) -> i64 {
    (i64::from(microseconds) + 99)
        .div_euclid(100)
        .clamp(span.min, span.max)
}

/// The driver's 100 us units to microseconds.
pub(crate) fn raw_to_exposure(raw: i64) -> u32 {
    u32::try_from(raw.saturating_mul(100).max(1)).unwrap_or(u32::MAX)
}

/// A position from 0 (nearest) to 1000 (farthest) to the driver's focus range.
pub(crate) fn focus_to_raw(position: u32, span: Span) -> i64 {
    let range = span.max - span.min;
    if range <= 0 {
        return span.min;
    }
    span.min + (i64::from(position.min(1000)) * range + 500) / 1000
}

/// The driver's focus value to a position from 0 to 1000.
pub(crate) fn raw_to_focus(raw: i64, span: Span) -> u32 {
    let range = span.max - span.min;
    if range <= 0 {
        return 0;
    }
    let scaled = (raw.clamp(span.min, span.max) - span.min) * 1000 + range / 2;
    u32::try_from((scaled / range).min(1000)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPAN: Span = Span {
        id: 0,
        min: 3,
        max: 2500,
    };

    #[test]
    fn exposure_converts_microseconds_to_hundred_microsecond_units() {
        assert_eq!(exposure_to_raw(10_000, SPAN), 100);
        // Rounded up: 10.01 ms needs 101 units.
        assert_eq!(exposure_to_raw(10_010, SPAN), 101);
        // Clamped to the driver's range.
        assert_eq!(exposure_to_raw(1, SPAN), 3);
        assert_eq!(exposure_to_raw(1_000_000, SPAN), 2500);
        assert_eq!(raw_to_exposure(100), 10_000);
    }

    #[test]
    fn focus_maps_between_zero_to_thousand_and_the_driver_range() {
        let span = Span {
            id: 0,
            min: 0,
            max: 255,
        };
        assert_eq!(focus_to_raw(0, span), 0);
        assert_eq!(focus_to_raw(1000, span), 255);
        assert_eq!(focus_to_raw(500, span), 128);
        assert_eq!(raw_to_focus(0, span), 0);
        assert_eq!(raw_to_focus(255, span), 1000);
        for raw in [0i64, 1, 77, 128, 254, 255] {
            let back = focus_to_raw(raw_to_focus(raw, span), span);
            assert!((back - raw).abs() <= 1, "{raw} came back as {back}");
        }
    }

    #[test]
    fn a_degenerate_range_is_one_value() {
        let span = Span {
            id: 0,
            min: 5,
            max: 5,
        };
        assert_eq!(focus_to_raw(700, span), 5);
        assert_eq!(raw_to_focus(5, span), 0);
    }
}
