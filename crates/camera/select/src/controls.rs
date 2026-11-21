//! A camera's controls as the camera has them: what [`Capture::controls_info`] answers.
//!
//! Two lists, both from the camera, never from a generic table:
//!
//! - [`StandardInfo`]: the standard controls ([`Control`]) this camera has, each in its documented
//!   styx-controls unit (exposure in microseconds, gain in thousandths, frame rate in millihertz,
//!   whatever unit the device itself uses), with this camera's range, step, default and the range it
//!   takes now. A standard control the camera does not have is not listed.
//! - [`DeviceInfo`]: every control the camera has, in the device's own units, by the backend's own
//!   id. The id is per backend (a V4L2 `V4L2_CID_*`, a libcamera control id, or, for a native
//!   Lemnos sensor, one of the ids below for the sensor contract's calls) and stable for a camera.
//!   `standard` names the standard control it maps to, if any: that name is the portable handle;
//!   the device list is everything.
//!
//! What a backend cannot say is `None` (shown as `unknown`): no range, step or default is ever made
//! up. Built when the camera opens and when its controls change, never per frame.
//!
//! [`Capture::controls_info`]: crate::Capture::controls_info

use styx_controls::{Control, ControlValue, DeviceKind};

/// The device control id of a native sensor's test pattern (the sensor contract's
/// `set_test_pattern`): a menu whose entry 0 is no pattern.
pub const NATIVE_TEST_PATTERN: u32 = 1;

/// Whether a control can be set now, and if not, why. A set of a control that is not
/// [`Access::ReadWrite`] is refused with this reason by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Access {
    /// It can be read and set.
    #[default]
    ReadWrite,
    /// The device refuses writes to it.
    ReadOnly,
    /// Not in effect under the current settings (V4L2 `INACTIVE`, for example a manual exposure
    /// while the exposure is automatic). The device may still take a value.
    Inactive,
    /// Held by another user of the device (V4L2 `GRABBED`, for example while streaming).
    Grabbed,
    /// Present but disabled (V4L2 `DISABLED`).
    Disabled,
}

impl Access {
    /// The word the control socket shows.
    pub fn name(self) -> &'static str {
        match self {
            Access::ReadWrite => "read_write",
            Access::ReadOnly => "read_only",
            Access::Inactive => "inactive",
            Access::Grabbed => "grabbed",
            Access::Disabled => "disabled",
        }
    }

    /// The access the word names.
    pub fn from_name(name: &str) -> Option<Access> {
        [
            Access::ReadWrite,
            Access::ReadOnly,
            Access::Inactive,
            Access::Grabbed,
            Access::Disabled,
        ]
        .into_iter()
        .find(|a| a.name() == name)
    }

    /// Whether a set is refused (every access but [`Access::ReadWrite`] and [`Access::Inactive`]:
    /// an inactive control is shown greyed, but the device takes a value for when it is in effect).
    pub fn refuses_set(self) -> bool {
        matches!(self, Access::ReadOnly | Access::Grabbed | Access::Disabled)
    }
}

/// A standard control's range on this camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// Numbers from `min` to `max`, in the control's styx-controls unit.
    Manual { min: u32, max: u32 },
    /// On or off.
    Flag,
    /// A rectangle (crop).
    Rect,
}

/// One standard control as this camera has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardInfo {
    pub control: Control,
    /// The camera's range, in the control's unit. `None`: the backend cannot say.
    pub range: Option<Span>,
    /// Between valid values, in the control's unit. `None`: not known or not even in this unit.
    pub step: Option<u32>,
    /// The camera's default. `None`: not known.
    pub default: Option<ControlValue>,
    /// The range the camera takes now (narrower than `range` under the current mode, for example
    /// exposure under the frame length in force). `None`: not known.
    pub effective: Option<(u32, u32)>,
    /// Whether the control has an automatic mode on this camera. `None`: not known.
    pub auto: Option<bool>,
    pub access: Access,
}

impl StandardInfo {
    /// `control` with nothing known about it on this camera.
    pub fn unknown(control: Control) -> Self {
        Self {
            control,
            range: None,
            step: None,
            default: None,
            effective: None,
            auto: None,
            access: Access::ReadWrite,
        }
    }
}

/// One entry of a menu control: the value that selects it, and its label where the device names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub value: i64,
    /// `None` when the device gives the entry no name (a native sensor's test patterns are
    /// numbered only). An integer menu's label is its number.
    pub label: Option<String>,
}

/// One control the camera has, in the device's own units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The backend's own id: stable for a camera, not portable across backends.
    pub id: u32,
    /// The device's name for it.
    pub name: String,
    pub kind: DeviceKind,
    /// Bounds, step and default as the device gives them. `None`: not given.
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub step: Option<u64>,
    pub default: Option<i64>,
    /// The range it takes now. `None`: not known.
    pub effective: Option<(i64, i64)>,
    /// The value as last read or set. `None`: not known.
    pub value: Option<i64>,
    /// The standard control this one is, when it maps to one: setting either moves the same value.
    pub standard: Option<Control>,
    pub access: Access,
    /// A menu's entries, in order; empty for other kinds.
    pub menu: Vec<MenuEntry>,
}

/// A camera's controls: the standard ones it has and every device control.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ControlsInfo {
    pub standard: Vec<StandardInfo>,
    pub device: Vec<DeviceInfo>,
}

impl ControlsInfo {
    /// What a backend that does not report its controls answers: every standard control, nothing
    /// known about any, and no device controls.
    pub fn unreported() -> Self {
        Self {
            standard: Control::ALL
                .into_iter()
                .map(StandardInfo::unknown)
                .collect(),
            device: Vec::new(),
        }
    }

    /// The standard control `control`, if the camera has it.
    pub fn standard(&self, control: Control) -> Option<&StandardInfo> {
        self.standard.iter().find(|s| s.control == control)
    }

    /// The device control `id`, if the camera has it.
    pub fn device(&self, id: u32) -> Option<&DeviceInfo> {
        self.device.iter().find(|d| d.id == id)
    }
}

/// Why a value is refused for a control before it reaches the camera. Only what the camera itself
/// says is checked: a bound it does not give is not checked here (the backend clamps, and the value
/// it applied shows afterwards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The control cannot be set now; the reason by name.
    Access(Access),
    /// Outside the camera's range (`min..max`; for a device control without bounds, the effective
    /// range). A value inside the range but past the effective range is taken: the camera clamps it
    /// and says so.
    OutOfRange { min: i64, max: i64, value: i64 },
    /// Between the camera's steps, counted from `min`.
    NotStep { min: i64, step: u64, value: i64 },
    /// A menu value that is none of its entries.
    NoEntry { value: i64 },
    /// The wrong kind of value: a number for a flag, a flag for a number, a value for a button.
    Kind,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Access(access) => write!(f, "the control is {}", access.name()),
            Refusal::OutOfRange { min, max, value } => {
                write!(f, "OutOfRange {{ min: {min}, max: {max}, value: {value} }}")
            }
            Refusal::NotStep { min, step, value } => {
                write!(f, "NotStep {{ min: {min}, step: {step}, value: {value} }}")
            }
            Refusal::NoEntry { value } => write!(f, "NoEntry {{ value: {value} }}"),
            Refusal::Kind => f.write_str("the value is the wrong kind for this control"),
        }
    }
}

/// `value` within `bounds` (when given), and on a step of `step` counted from `base`.
fn check_number(
    value: i64,
    bounds: Option<(i64, i64)>,
    base: Option<i64>,
    step: Option<u64>,
) -> Result<(), Refusal> {
    if let Some((min, max)) = bounds
        && (value < min || value > max)
    {
        return Err(Refusal::OutOfRange { min, max, value });
    }
    if let (Some(base), Some(step)) = (base, step.filter(|s| *s > 1))
        && !(value - base).unsigned_abs().is_multiple_of(step)
    {
        return Err(Refusal::NotStep {
            min: base,
            step,
            value,
        });
    }
    Ok(())
}

impl StandardInfo {
    /// Checks `value` against what this camera says of the control. A bound it does not give is
    /// not checked.
    pub fn check(&self, value: ControlValue) -> Result<(), Refusal> {
        if self.access.refuses_set() {
            return Err(Refusal::Access(self.access));
        }
        match (value, self.range) {
            (ControlValue::Auto, _) => match self.auto {
                Some(false) => Err(Refusal::Kind),
                _ => Ok(()),
            },
            (ControlValue::Manual(v), Some(Span::Manual { min, max })) => check_number(
                i64::from(v),
                Some((i64::from(min), i64::from(max))),
                Some(i64::from(min)),
                self.step.map(u64::from),
            ),
            (ControlValue::Manual(_), None) => Ok(()),
            (ControlValue::Manual(_), Some(Span::Flag | Span::Rect))
            | (ControlValue::Flag(_), Some(Span::Manual { .. } | Span::Rect))
            | (ControlValue::Crop(_), Some(Span::Manual { .. } | Span::Flag)) => Err(Refusal::Kind),
            _ => Ok(()),
        }
    }
}

impl DeviceInfo {
    /// Checks `value` against what the device says of the control. A bound it does not give is not
    /// checked.
    pub fn check(&self, value: i64) -> Result<(), Refusal> {
        if self.access.refuses_set() {
            return Err(Refusal::Access(self.access));
        }
        match self.kind {
            DeviceKind::Menu => {
                if self.menu.is_empty() || self.menu.iter().any(|e| e.value == value) {
                    Ok(())
                } else {
                    Err(Refusal::NoEntry { value })
                }
            }
            DeviceKind::Boolean if value != 0 && value != 1 => Err(Refusal::Kind),
            DeviceKind::Other => Err(Refusal::Kind),
            DeviceKind::Bitmask | DeviceKind::Button => Ok(()),
            _ => {
                let bounds = match (self.min, self.max) {
                    (Some(min), Some(max)) => Some((min, max)),
                    _ => self.effective,
                };
                check_number(value, bounds, self.min, self.step)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exposure() -> StandardInfo {
        StandardInfo {
            control: Control::Exposure,
            range: Some(Span::Manual { min: 10, max: 1000 }),
            step: Some(10),
            default: Some(ControlValue::Manual(100)),
            effective: Some((10, 500)),
            auto: Some(false),
            access: Access::ReadWrite,
        }
    }

    fn menu() -> DeviceInfo {
        DeviceInfo {
            id: 0x0098_0918,
            name: "Power Line Frequency".to_string(),
            kind: DeviceKind::Menu,
            min: Some(0),
            max: Some(2),
            step: Some(1),
            default: Some(1),
            effective: None,
            value: Some(1),
            standard: Some(Control::PowerLineFrequency),
            access: Access::ReadWrite,
            menu: vec![
                MenuEntry {
                    value: 0,
                    label: Some("Disabled".to_string()),
                },
                MenuEntry {
                    value: 2,
                    label: Some("60 Hz".to_string()),
                },
            ],
        }
    }

    #[test]
    fn a_standard_value_is_checked_against_the_cameras_range_and_steps_from_its_minimum() {
        let info = exposure();
        assert_eq!(info.check(ControlValue::Manual(500)), Ok(()));
        assert_eq!(
            info.check(ControlValue::Manual(510)),
            Ok(()),
            "past the effective ceiling, inside the camera's range: the camera clamps it"
        );
        assert_eq!(
            info.check(ControlValue::Manual(1010)),
            Err(Refusal::OutOfRange {
                min: 10,
                max: 1000,
                value: 1010
            })
        );
        assert_eq!(
            info.check(ControlValue::Manual(15)),
            Err(Refusal::NotStep {
                min: 10,
                step: 10,
                value: 15
            })
        );
        assert_eq!(
            info.check(ControlValue::Auto),
            Err(Refusal::Kind),
            "no automatic mode"
        );
        assert_eq!(info.check(ControlValue::Flag(true)), Err(Refusal::Kind));
    }

    #[test]
    fn what_the_camera_does_not_say_is_not_checked() {
        let info = StandardInfo::unknown(Control::Exposure);
        for value in [0, 1, u32::MAX] {
            assert_eq!(info.check(ControlValue::Manual(value)), Ok(()));
        }
        assert_eq!(info.check(ControlValue::Auto), Ok(()));
        let device = DeviceInfo {
            min: None,
            max: None,
            step: None,
            kind: DeviceKind::Integer,
            menu: Vec::new(),
            ..menu()
        };
        assert_eq!(device.check(i64::MIN), Ok(()));
    }

    #[test]
    fn a_control_that_cannot_be_set_is_refused_by_its_access_and_an_inactive_one_is_taken() {
        for access in [Access::ReadOnly, Access::Grabbed, Access::Disabled] {
            let info = StandardInfo {
                access,
                ..exposure()
            };
            assert_eq!(
                info.check(ControlValue::Manual(100)),
                Err(Refusal::Access(access))
            );
            let device = DeviceInfo { access, ..menu() };
            assert_eq!(device.check(0), Err(Refusal::Access(access)));
            assert_eq!(
                Refusal::Access(access).to_string(),
                format!("the control is {}", access.name())
            );
        }
        let inactive = StandardInfo {
            access: Access::Inactive,
            ..exposure()
        };
        assert_eq!(inactive.check(ControlValue::Manual(100)), Ok(()));
        for access in [
            Access::ReadWrite,
            Access::ReadOnly,
            Access::Inactive,
            Access::Grabbed,
            Access::Disabled,
        ] {
            assert_eq!(Access::from_name(access.name()), Some(access));
        }
    }

    #[test]
    fn a_menu_takes_its_entries_only_and_a_boolean_zero_or_one() {
        let info = menu();
        assert_eq!(info.check(0), Ok(()));
        assert_eq!(info.check(2), Ok(()));
        assert_eq!(
            info.check(1),
            Err(Refusal::NoEntry { value: 1 }),
            "a gap in a sparse menu"
        );
        let flag = DeviceInfo {
            kind: DeviceKind::Boolean,
            menu: Vec::new(),
            ..menu()
        };
        assert_eq!(flag.check(1), Ok(()));
        assert_eq!(flag.check(2), Err(Refusal::Kind));
    }

    #[test]
    fn an_unreported_backend_lists_every_standard_control_with_nothing_known() {
        let info = ControlsInfo::unreported();
        assert_eq!(info.standard.len(), Control::COUNT);
        assert!(
            info.standard
                .iter()
                .all(|s| *s == StandardInfo::unknown(s.control))
        );
        assert!(info.device.is_empty());
    }
}
