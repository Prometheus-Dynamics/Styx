//! The control set, its ranges and defaults, and the values it takes.

use core::fmt;

/// A frame's identity: the [`styx_frame::FrameEntry::id`] a value applies to. Styx gives it no
/// meaning beyond equality and order.
pub type FrameId = u64;

/// The controls every backend maps to. Their order is [`Control::ALL`]; [`Control::index`] is the
/// position in [`FrameMetadata::controls`](crate::FrameMetadata::controls).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Control {
    /// Exposure time, in microseconds, or automatic.
    Exposure,
    /// Analogue gain, in thousandths (1000 is 1.0x), or automatic.
    Gain,
    /// Colour temperature of the white balance, in kelvin, or automatic.
    WhiteBalance,
    /// Focus position, normalised to 0 (nearest) through 1000 (farthest), or automatic.
    Focus,
    /// Frame rate, in millihertz.
    FrameRate,
    /// Mirror the image left to right.
    HFlip,
    /// Mirror the image top to bottom.
    VFlip,
    /// Crop of the sensor's pixel array, or the whole array.
    Crop,
}

impl Control {
    /// Number of controls in the set.
    pub const COUNT: usize = 8;

    /// Every control, in index order.
    pub const ALL: [Control; Self::COUNT] = [
        Control::Exposure,
        Control::Gain,
        Control::WhiteBalance,
        Control::Focus,
        Control::FrameRate,
        Control::HFlip,
        Control::VFlip,
        Control::Crop,
    ];

    /// The position of this control in [`Control::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The control's range, default and automatic mode.
    pub const fn spec(self) -> Spec {
        match self {
            Control::Exposure => Spec {
                control: self,
                unit: "us",
                range: Range::Manual {
                    min: 1,
                    max: 1_000_000,
                },
                default: ControlValue::Auto,
                auto: true,
            },
            Control::Gain => Spec {
                control: self,
                unit: "milli",
                range: Range::Manual {
                    min: 1_000,
                    max: 16_000,
                },
                default: ControlValue::Auto,
                auto: true,
            },
            Control::WhiteBalance => Spec {
                control: self,
                unit: "K",
                range: Range::Manual {
                    min: 1_500,
                    max: 12_000,
                },
                default: ControlValue::Auto,
                auto: true,
            },
            Control::Focus => Spec {
                control: self,
                unit: "position",
                range: Range::Manual { min: 0, max: 1_000 },
                default: ControlValue::Auto,
                auto: true,
            },
            Control::FrameRate => Spec {
                control: self,
                unit: "mHz",
                range: Range::Manual {
                    min: 1_000,
                    max: 240_000,
                },
                default: ControlValue::Manual(30_000),
                auto: false,
            },
            Control::HFlip | Control::VFlip => Spec {
                control: self,
                unit: "",
                range: Range::Flag,
                default: ControlValue::Flag(false),
                auto: false,
            },
            Control::Crop => Spec {
                control: self,
                unit: "px",
                range: Range::Rect,
                default: ControlValue::Crop(None),
                auto: false,
            },
        }
    }

    /// The control's name as it appears in configuration and metadata.
    pub const fn name(self) -> &'static str {
        match self {
            Control::Exposure => "exposure",
            Control::Gain => "gain",
            Control::WhiteBalance => "white_balance",
            Control::Focus => "focus",
            Control::FrameRate => "frame_rate",
            Control::HFlip => "h_flip",
            Control::VFlip => "v_flip",
            Control::Crop => "crop",
        }
    }

    /// Checks that `value` is one this control takes: the right kind, in range, and `Auto` only
    /// where the control has an automatic mode.
    pub fn validate(self, value: ControlValue) -> Result<(), ValueError> {
        let spec = self.spec();
        match (spec.range, value) {
            (_, ControlValue::Auto) if spec.auto => Ok(()),
            (_, ControlValue::Auto) => Err(ValueError::NoAuto),
            (Range::Manual { min, max }, ControlValue::Manual(v)) => {
                if v < min || v > max {
                    Err(ValueError::OutOfRange { min, max, value: v })
                } else {
                    Ok(())
                }
            }
            (Range::Flag, ControlValue::Flag(_)) => Ok(()),
            (Range::Rect, ControlValue::Crop(None)) => Ok(()),
            (Range::Rect, ControlValue::Crop(Some(r))) => {
                if r.width == 0 || r.height == 0 {
                    Err(ValueError::EmptyCrop)
                } else {
                    Ok(())
                }
            }
            _ => Err(ValueError::WrongKind),
        }
    }
}

/// How a control's values are bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Range {
    /// Manual values from `min` through `max`, both inclusive.
    Manual { min: u32, max: u32 },
    /// On or off.
    Flag,
    /// A rectangle of the sensor's pixel array with non-zero size. Its upper bound is the array,
    /// which a backend reports at run time, so it is not fixed here.
    Rect,
}

/// A control's range, default and automatic mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Spec {
    /// The control this describes.
    pub control: Control,
    /// The unit the manual values are in (see the crate docs). Empty for flags and crops.
    pub unit: &'static str,
    /// How manual values are bounded.
    pub range: Range,
    /// The value in force when nothing has been set.
    pub default: ControlValue,
    /// Whether [`ControlValue::Auto`] is accepted.
    pub auto: bool,
}

/// A rectangle in pixels of the sensor's pixel array.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A value of one control. Every variant is plain data; the variant must match the control's
/// [`Range`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ControlValue {
    /// The backend picks the value (only where [`Spec::auto`] is set).
    #[default]
    Auto,
    /// A manual value in the control's unit.
    Manual(u32),
    /// A flag (flips).
    Flag(bool),
    /// A crop: `None` is the whole pixel array.
    Crop(Option<Rect>),
}

/// A value and the frame it first applies to. A backend that has a sensor delay posts the value
/// for the frame id it computed; the value stays in force from there until another update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Update {
    /// The control being set.
    pub control: Control,
    /// The value.
    pub value: ControlValue,
    /// The first frame (by [`FrameId`]) this value applies to.
    pub frame: FrameId,
}

/// Why a value was refused for a control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum ValueError {
    /// The value's kind does not match the control's range (for example a flag for exposure).
    WrongKind,
    /// `Auto` was given for a control without an automatic mode.
    NoAuto,
    /// A manual value outside the control's range.
    OutOfRange { min: u32, max: u32, value: u32 },
    /// A crop with zero width or height.
    EmptyCrop,
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueError::WrongKind => f.write_str("value kind does not match the control"),
            ValueError::NoAuto => f.write_str("control has no automatic mode"),
            ValueError::OutOfRange { min, max, value } => {
                write!(f, "value {value} is outside {min}..={max}")
            }
            ValueError::EmptyCrop => f.write_str("crop has zero width or height"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ValueError {}
