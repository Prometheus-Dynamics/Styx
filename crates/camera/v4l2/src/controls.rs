//! Styx's controls on a node's V4L2 controls. Each mapped control has one conversion, both ways.
//!
//! Exposure is in microseconds here and in 100 us units on the wire (the V4L2 unit of
//! `V4L2_CID_EXPOSURE_ABSOLUTE`). Focus, and every normalised control, is 0 to 1000 here and the
//! driver's own range there. Pan and tilt are arc-seconds on both sides, centred on 648000.
//! Frame rate has no V4L2 control: it is the frame interval. Gain and crop are not mapped: gain's
//! unit is the driver's, and crop is the selection API, not a control. A control with no mapping
//! here is a passthrough (see [`crate::RawControl`]).

use styx_controls::{Control, ControlValue};
use styx_lemnos::v4l2::Fraction;
use styx_lemnos::v4l2::v4l2::{
    BufType, ControlInfo, ControlType, ControlValue as Raw, Controls, VideoDevice, cid,
};

use crate::Error;

/// The centre of the arc-second scale for pan and tilt: 0 degrees.
const ARC_CENTRE: i64 = 648_000;

/// A driver control's integer range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) id: u32,
    pub(crate) min: i64,
    pub(crate) max: i64,
}

/// What a Styx value means on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scale {
    /// `lo..=hi` maps linearly onto the driver's range, rounded to the nearest value.
    Linear { lo: u32, hi: u32 },
    /// The driver's value is the Styx value (kelvin, a menu index).
    Direct,
    /// Arc-seconds about 0 degrees: the driver's value is the Styx value less `ARC_CENTRE`.
    Centred,
}

/// The normalised scale: 0 through 1000.
const NORMALISED: Scale = Scale::Linear { lo: 0, hi: 1_000 };

/// The shape of the driver control a Styx control maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// An integer control (`Integer`).
    Integer,
    /// A menu, named or integer; its value is an item index.
    Menu,
    /// A boolean control.
    Flag,
}

impl Shape {
    fn matches(self, control_type: ControlType) -> bool {
        match self {
            Shape::Integer => matches!(control_type, ControlType::Integer),
            Shape::Menu => matches!(control_type, ControlType::Menu | ControlType::IntegerMenu),
            Shape::Flag => matches!(control_type, ControlType::Boolean),
        }
    }
}

/// How one generic Styx control maps: the driver control, its shape and its scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub(crate) id: u32,
    pub(crate) shape: Shape,
    pub(crate) scale: Scale,
}

/// The mapping of each Styx control that is a plain V4L2 control. Exposure, frame rate, gain and
/// crop are not here: exposure has its own unit and mode menu, frame rate is the interval, gain
/// and crop are not mapped.
pub(crate) fn mapping(control: Control) -> Option<Mapping> {
    let (id, shape, scale) = match control {
        Control::Brightness => (cid::BRIGHTNESS, Shape::Integer, NORMALISED),
        Control::Contrast => (cid::CONTRAST, Shape::Integer, NORMALISED),
        Control::Saturation => (cid::SATURATION, Shape::Integer, NORMALISED),
        Control::Hue => (cid::HUE, Shape::Integer, NORMALISED),
        Control::Sharpness => (cid::SHARPNESS, Shape::Integer, NORMALISED),
        Control::Gamma => (
            cid::GAMMA,
            Shape::Integer,
            Scale::Linear {
                lo: 1_000,
                hi: 3_000,
            },
        ),
        Control::BacklightCompensation => (cid::BACKLIGHT_COMPENSATION, Shape::Integer, NORMALISED),
        Control::PowerLineFrequency => (cid::POWER_LINE_FREQUENCY, Shape::Menu, Scale::Direct),
        Control::Zoom => (cid::ZOOM_ABSOLUTE, Shape::Integer, NORMALISED),
        Control::Pan => (cid::PAN_ABSOLUTE, Shape::Integer, Scale::Centred),
        Control::Tilt => (cid::TILT_ABSOLUTE, Shape::Integer, Scale::Centred),
        Control::Focus => (cid::FOCUS_ABSOLUTE, Shape::Integer, NORMALISED),
        Control::WhiteBalance => (
            cid::WHITE_BALANCE_TEMPERATURE,
            Shape::Integer,
            Scale::Direct,
        ),
        Control::HFlip => (cid::HFLIP, Shape::Flag, Scale::Direct),
        Control::VFlip => (cid::VFLIP, Shape::Flag, Scale::Direct),
        Control::Exposure | Control::Gain | Control::FrameRate | Control::Crop => return None,
    };
    Some(Mapping { id, shape, scale })
}

/// Every driver control a Styx control uses. The automatic-mode switches of exposure, white
/// balance and focus count as part of their Styx control, so a node's listing marks them mapped.
pub(crate) fn driver_controls(control: Control) -> &'static [u32] {
    match control {
        Control::Exposure => &[cid::EXPOSURE_ABSOLUTE, cid::EXPOSURE_AUTO],
        Control::WhiteBalance => &[cid::WHITE_BALANCE_TEMPERATURE, cid::AUTO_WHITE_BALANCE],
        Control::Focus => &[cid::FOCUS_ABSOLUTE, cid::FOCUS_AUTO],
        Control::Brightness => &[cid::BRIGHTNESS],
        Control::Contrast => &[cid::CONTRAST],
        Control::Saturation => &[cid::SATURATION],
        Control::Hue => &[cid::HUE],
        Control::Sharpness => &[cid::SHARPNESS],
        Control::Gamma => &[cid::GAMMA],
        Control::BacklightCompensation => &[cid::BACKLIGHT_COMPENSATION],
        Control::PowerLineFrequency => &[cid::POWER_LINE_FREQUENCY],
        Control::Zoom => &[cid::ZOOM_ABSOLUTE],
        Control::Pan => &[cid::PAN_ABSOLUTE],
        Control::Tilt => &[cid::TILT_ABSOLUTE],
        Control::HFlip => &[cid::HFLIP],
        Control::VFlip => &[cid::VFLIP],
        Control::Gain | Control::FrameRate | Control::Crop => &[],
    }
}

/// The Styx control a driver control belongs to, when this crate maps it.
pub(crate) fn control_of(id: u32) -> Option<Control> {
    Control::ALL
        .into_iter()
        .find(|&control| driver_controls(control).contains(&id))
}

/// The exposure mode menu: `manual` turns automatic exposure off, `auto` turns it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExposureMode {
    id: u32,
    manual: i32,
    auto: i32,
}

/// The controls one node maps, as they were probed. `Copy`, so a stream keeps its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Table {
    exposure: Option<Span>,
    exposure_mode: Option<ExposureMode>,
    white_balance_auto: Option<u32>,
    focus_auto: Option<u32>,
    frame_rate: bool,
    /// The generic mapped controls, by [`Control::index`], with their shape and scale.
    slots: [Option<Slot>; Control::COUNT],
}

impl Default for Table {
    fn default() -> Self {
        Table {
            exposure: None,
            exposure_mode: None,
            white_balance_auto: None,
            focus_auto: None,
            frame_rate: false,
            slots: [None; Control::COUNT],
        }
    }
}

/// One generic mapped control on a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    span: Span,
    scale: Scale,
    shape: Shape,
}

impl Table {
    /// The mapped controls of `dev`, given its probed controls `infos`, with the queue `buf_type`
    /// for the frame rate. A node without control support has an empty table.
    pub(crate) fn from_infos(dev: &VideoDevice, infos: &[ControlInfo], buf_type: BufType) -> Table {
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
        let mut slots = [None; Control::COUNT];
        for control in Control::ALL {
            let Some(mapping) = mapping(control) else {
                continue;
            };
            let Some(info) = find(mapping.id) else {
                continue;
            };
            if !mapping.shape.matches(info.control_type) {
                continue;
            }
            slots[control.index()] = Some(Slot {
                span: Span {
                    id: mapping.id,
                    min: info.minimum,
                    max: info.maximum,
                },
                scale: mapping.scale,
                shape: mapping.shape,
            });
        }
        Table {
            exposure: integer(cid::EXPOSURE_ABSOLUTE),
            exposure_mode: find(cid::EXPOSURE_AUTO).and_then(|info| exposure_mode(dev, info)),
            white_balance_auto: flag(cid::AUTO_WHITE_BALANCE),
            focus_auto: flag(cid::FOCUS_AUTO),
            frame_rate: dev.stream_params(buf_type).is_ok(),
            slots,
        }
    }

    /// Whether this node maps `control`.
    pub(crate) fn supports(&self, control: Control) -> bool {
        match control {
            Control::Exposure => self.exposure.is_some(),
            Control::FrameRate => self.frame_rate,
            Control::Gain | Control::Crop => false,
            Control::WhiteBalance => self.slots[control.index()].is_some(),
            Control::Focus => self.slots[control.index()].is_some(),
            other => self.slots[other.index()].is_some(),
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
            (Control::Focus, ControlValue::Auto) => {
                let id = self.focus_auto.ok_or_else(|| {
                    Error::Unsupported("focus has no automatic mode on this node".into())
                })?;
                dev.set_control(id, Raw::Integer(1))?;
            }
            (Control::WhiteBalance, ControlValue::Manual(_))
            | (Control::Focus, ControlValue::Manual(_)) => {
                if let Some(id) = match control {
                    Control::WhiteBalance => self.white_balance_auto,
                    _ => self.focus_auto,
                } {
                    dev.set_control(id, Raw::Integer(0))?;
                }
                let slot = self.slot(control)?;
                dev.set_control(slot.span.id, Raw::Integer(to_i32(raw_of(slot, value))))?;
            }
            (Control::FrameRate, ControlValue::Manual(millihertz)) => {
                // Seconds per frame is 1000 / millihertz, validated to be at least 1000 mHz.
                dev.set_frame_interval(buf_type, Fraction::new(1000, millihertz))?;
            }
            (_, ControlValue::Flag(on)) => {
                let slot = self.slot(control)?;
                dev.set_control(slot.span.id, Raw::Integer(i32::from(on)))?;
            }
            (_, ControlValue::Manual(_)) => {
                let slot = self.slot(control)?;
                dev.set_control(slot.span.id, Raw::Integer(to_i32(raw_of(slot, value))))?;
            }
            _ => return Err(Error::NoControl(control)),
        }
        Ok(())
    }

    fn slot(&self, control: Control) -> Result<Slot, Error> {
        self.slots[control.index()].ok_or(Error::NoControl(control))
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
                let slot = self.slot(control).ok()?;
                if let Some(id) = self.white_balance_auto
                    && read_int(dev, id)? != 0
                {
                    return Some(ControlValue::Auto);
                }
                Some(ControlValue::Manual(styx_of(
                    slot,
                    read_int(dev, slot.span.id)?,
                )))
            }
            Control::Focus => {
                let slot = self.slot(control).ok()?;
                if let Some(id) = self.focus_auto
                    && read_int(dev, id)? != 0
                {
                    return Some(ControlValue::Auto);
                }
                Some(ControlValue::Manual(styx_of(
                    slot,
                    read_int(dev, slot.span.id)?,
                )))
            }
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
            other => {
                let slot = self.slot(other).ok()?;
                let raw = read_int(dev, slot.span.id)?;
                match slot.shape {
                    Shape::Flag => Some(ControlValue::Flag(raw != 0)),
                    Shape::Integer | Shape::Menu => Some(ControlValue::Manual(styx_of(slot, raw))),
                }
            }
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

/// The driver's value for a Styx value of a generic control, within the driver's range.
fn raw_of(slot: Slot, value: ControlValue) -> i64 {
    let ControlValue::Manual(v) = value else {
        return slot.span.min;
    };
    to_raw(v, slot.span, slot.scale)
}

/// The Styx value of a driver value of a generic control.
fn styx_of(slot: Slot, raw: i64) -> u32 {
    from_raw(raw, slot.span, slot.scale)
}

/// A Styx value to the driver's value, clamped to the driver's range.
pub(crate) fn to_raw(value: u32, span: Span, scale: Scale) -> i64 {
    match scale {
        Scale::Direct => i64::from(value).clamp(span.min, span.max),
        Scale::Centred => (i64::from(value) - ARC_CENTRE).clamp(span.min, span.max),
        Scale::Linear { lo, hi } => {
            let v = i128::from(value.clamp(lo, hi)) - i128::from(lo);
            let styx_span = i128::from(hi) - i128::from(lo);
            let driver_span = i128::from(span.max) - i128::from(span.min);
            if styx_span <= 0 || driver_span <= 0 {
                return span.min;
            }
            let raw = i128::from(span.min) + (v * driver_span + styx_span / 2) / styx_span;
            raw.clamp(i128::from(span.min), i128::from(span.max)) as i64
        }
    }
}

/// The driver's value to the Styx value, within the Styx range.
pub(crate) fn from_raw(raw: i64, span: Span, scale: Scale) -> u32 {
    match scale {
        Scale::Direct => u32::try_from(raw.max(0)).unwrap_or(u32::MAX),
        Scale::Centred => u32::try_from((raw + ARC_CENTRE).max(0)).unwrap_or(u32::MAX),
        Scale::Linear { lo, hi } => {
            let driver_span = i128::from(span.max) - i128::from(span.min);
            if driver_span <= 0 || hi <= lo {
                return lo;
            }
            let r = i128::from(raw.clamp(span.min, span.max)) - i128::from(span.min);
            let styx_span = i128::from(hi) - i128::from(lo);
            let v = i128::from(lo) + (r * styx_span + driver_span / 2) / driver_span;
            v.clamp(i128::from(lo), i128::from(hi)) as u32
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use styx_controls::PowerLineFrequency;

    /// A focus position from 0 (nearest) to 1000 (farthest) to the driver's focus range.
    fn focus_to_raw(position: u32, span: Span) -> i64 {
        to_raw(position, span, NORMALISED)
    }

    /// The driver's focus value to a position from 0 to 1000.
    fn raw_to_focus(raw: i64, span: Span) -> u32 {
        from_raw(raw, span, NORMALISED)
    }

    /// The power line frequency a driver menu index selects.
    fn power_line_of(index: i64) -> Option<PowerLineFrequency> {
        u32::try_from(index)
            .ok()
            .and_then(PowerLineFrequency::from_value)
    }

    /// The driver's control ids this crate maps, one per Styx control, in [`Control::ALL`] order.
    fn mapped_ids() -> Vec<(Control, u32)> {
        Control::ALL
            .into_iter()
            .filter_map(|control| match control {
                Control::Exposure => Some((control, cid::EXPOSURE_ABSOLUTE)),
                Control::WhiteBalance => Some((control, cid::WHITE_BALANCE_TEMPERATURE)),
                Control::Focus => Some((control, cid::FOCUS_ABSOLUTE)),
                _ => mapping(control).map(|m| (control, m.id)),
            })
            .collect()
    }

    const SPAN: Span = Span {
        id: 0,
        min: 3,
        max: 2500,
    };

    fn span(min: i64, max: i64) -> Span {
        Span { id: 0, min, max }
    }

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
        let span = span(0, 255);
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
        let span = span(5, 5);
        assert_eq!(focus_to_raw(700, span), 5);
        assert_eq!(raw_to_focus(5, span), 0);
    }

    #[test]
    fn normalised_controls_map_their_ends_and_centre() {
        // A symmetric driver range (hue -128..=128) puts 0 degrees at 500.
        let hue = span(-128, 128);
        assert_eq!(to_raw(0, hue, NORMALISED), -128);
        assert_eq!(to_raw(1_000, hue, NORMALISED), 128);
        assert_eq!(to_raw(500, hue, NORMALISED), 0);
        assert_eq!(from_raw(0, hue, NORMALISED), 500);
        // Brightness on a 0..=255 range (vivid's, UVC's): default 128 reads back near 500.
        let bright = span(0, 255);
        assert_eq!(from_raw(128, bright, NORMALISED), 502);
        assert_eq!(to_raw(500, bright, NORMALISED), 128);
    }

    #[test]
    fn normalised_values_round_trip_within_a_step() {
        for driver in [span(0, 255), span(-128, 128), span(0, 1), span(10, 110)] {
            for v in (0..=1_000u32).step_by(7) {
                let raw = to_raw(v, driver, NORMALISED);
                assert!(raw >= driver.min && raw <= driver.max);
                let back = from_raw(raw, driver, NORMALISED);
                let step = (1_000 / (driver.max - driver.min).max(1)) as u32 + 1;
                assert!(
                    back.abs_diff(v) <= step,
                    "{v} came back {back} on {driver:?}"
                );
            }
        }
    }

    #[test]
    fn out_of_range_styx_values_clamp_to_the_driver_range() {
        let bright = span(0, 255);
        assert_eq!(to_raw(5_000, bright, NORMALISED), 255);
        assert_eq!(from_raw(-40, bright, NORMALISED), 0);
        assert_eq!(from_raw(9_000, bright, NORMALISED), 1_000);
    }

    #[test]
    fn gamma_is_linear_from_one_to_three_in_thousandths() {
        let gamma = Scale::Linear {
            lo: 1_000,
            hi: 3_000,
        };
        // A driver range of 72..=500 (UVC gamma in hundredths): its ends are the Styx ends.
        let driver = span(72, 500);
        assert_eq!(to_raw(1_000, driver, gamma), 72);
        assert_eq!(to_raw(3_000, driver, gamma), 500);
        assert_eq!(from_raw(72, driver, gamma), 1_000);
        assert_eq!(from_raw(500, driver, gamma), 3_000);
        let mid = to_raw(2_000, driver, gamma);
        assert!(from_raw(mid, driver, gamma).abs_diff(2_000) <= 15);
    }

    #[test]
    fn pan_and_tilt_are_arc_seconds_about_the_centre() {
        // A UVC driver reports pan in arc-seconds, -180..=180 degrees about 0.
        let pan = span(-648_000, 648_000);
        assert_eq!(to_raw(648_000, pan, Scale::Centred), 0);
        assert_eq!(to_raw(0, pan, Scale::Centred), -648_000);
        assert_eq!(to_raw(1_296_000, pan, Scale::Centred), 648_000);
        assert_eq!(from_raw(0, pan, Scale::Centred), 648_000);
        assert_eq!(from_raw(-648_000, pan, Scale::Centred), 0);
        // Past the driver's range the value clamps.
        assert_eq!(
            to_raw(1_296_000, span(-36_000, 36_000), Scale::Centred),
            36_000
        );
    }

    #[test]
    fn direct_values_are_the_driver_value_within_its_range() {
        let kelvin = span(2_800, 6_500);
        assert_eq!(to_raw(4_500, kelvin, Scale::Direct), 4_500);
        assert_eq!(to_raw(100, kelvin, Scale::Direct), 2_800);
        assert_eq!(from_raw(4_500, kelvin, Scale::Direct), 4_500);
    }

    #[test]
    fn power_line_menu_indices_are_the_styx_values() {
        assert_eq!(power_line_of(0), Some(PowerLineFrequency::Disabled));
        assert_eq!(power_line_of(1), Some(PowerLineFrequency::Hz50));
        assert_eq!(power_line_of(2), Some(PowerLineFrequency::Hz60));
        assert_eq!(power_line_of(3), Some(PowerLineFrequency::Auto));
        assert_eq!(power_line_of(4), None);
        assert_eq!(power_line_of(-1), None);
    }

    #[test]
    fn every_named_control_maps_to_its_lemnos_constant() {
        let expect = [
            (Control::Brightness, cid::BRIGHTNESS),
            (Control::Contrast, cid::CONTRAST),
            (Control::Saturation, cid::SATURATION),
            (Control::Hue, cid::HUE),
            (Control::Sharpness, cid::SHARPNESS),
            (Control::Gamma, cid::GAMMA),
            (Control::BacklightCompensation, cid::BACKLIGHT_COMPENSATION),
            (Control::PowerLineFrequency, cid::POWER_LINE_FREQUENCY),
            (Control::Zoom, cid::ZOOM_ABSOLUTE),
            (Control::Pan, cid::PAN_ABSOLUTE),
            (Control::Tilt, cid::TILT_ABSOLUTE),
            (Control::Exposure, cid::EXPOSURE_ABSOLUTE),
            (Control::WhiteBalance, cid::WHITE_BALANCE_TEMPERATURE),
            (Control::Focus, cid::FOCUS_ABSOLUTE),
            (Control::HFlip, cid::HFLIP),
            (Control::VFlip, cid::VFLIP),
        ];
        let mapped = mapped_ids();
        assert_eq!(mapped.len(), expect.len(), "mapped: {mapped:?}");
        for (control, id) in expect {
            assert!(
                mapped.contains(&(control, id)),
                "{} is not mapped to {id:#010x}",
                control.name()
            );
        }
    }

    #[test]
    fn no_driver_control_belongs_to_two_styx_controls() {
        let mut seen: Vec<(u32, Control)> = Vec::new();
        for control in Control::ALL {
            for &id in driver_controls(control) {
                if let Some(&(_, other)) = seen.iter().find(|(seen_id, _)| *seen_id == id) {
                    panic!(
                        "{id:#010x} maps to both {} and {}",
                        other.name(),
                        control.name()
                    );
                }
                seen.push((id, control));
            }
        }
    }

    #[test]
    fn the_generic_mappings_agree_with_the_driver_control_lists() {
        for control in Control::ALL {
            let Some(mapping) = mapping(control) else {
                continue;
            };
            if matches!(control, Control::WhiteBalance | Control::Focus) {
                continue;
            }
            assert_eq!(
                driver_controls(control),
                &[mapping.id],
                "{}",
                control.name()
            );
        }
    }

    #[test]
    fn gain_and_crop_are_left_to_passthrough() {
        assert!(mapping(Control::Gain).is_none());
        assert!(mapping(Control::Crop).is_none());
        assert!(driver_controls(Control::Gain).is_empty());
        assert_eq!(control_of(cid::GAIN), None);
        assert_eq!(control_of(cid::AUTOGAIN), None);
        assert_eq!(
            control_of(cid::AUTO_WHITE_BALANCE),
            Some(Control::WhiteBalance)
        );
        assert_eq!(control_of(cid::FOCUS_AUTO), Some(Control::Focus));
        assert_eq!(control_of(cid::EXPOSURE_AUTO), Some(Control::Exposure));
        assert_eq!(control_of(cid::HUE), Some(Control::Hue));
    }
}
