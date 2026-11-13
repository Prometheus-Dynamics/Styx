//! The control set, its ranges and defaults, and the values it takes.

use core::fmt;

/// A frame's identity: the [`styx_frame::FrameEntry::id`] a value applies to. Styx gives it no
/// meaning beyond equality and order.
pub type FrameId = u64;

/// The controls every backend maps to. Their order is [`Control::ALL`]; [`Control::index`] is the
/// position in [`FrameMetadata::controls`](crate::FrameMetadata::controls). A backend maps its own
/// control ids onto these where they mean the same thing; anything else it offers goes through
/// [`DeviceControl`] instead.
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
    /// Brightness, normalised 0 (darkest) through 1000, 500 neutral when the device range is
    /// symmetric.
    Brightness,
    /// Contrast, normalised 0 through 1000, 500 neutral when the device range is symmetric.
    Contrast,
    /// Saturation, normalised 0 through 1000, 500 neutral when the device range is symmetric.
    Saturation,
    /// Hue, normalised 0 through 1000, 500 neutral (0 degrees) when the device range is
    /// symmetric about 0.
    Hue,
    /// Sharpness, normalised 0 through 1000, 500 neutral when the device range is symmetric.
    Sharpness,
    /// Gamma in thousandths: 2200 is 2.2. Manual values run 1000 (1.0) through 3000 (3.0).
    Gamma,
    /// Backlight compensation, normalised 0 (off) through 1000.
    BacklightCompensation,
    /// Power line frequency filter: a [`PowerLineFrequency`] value, 0 through 3.
    PowerLineFrequency,
    /// Zoom, normalised 0 (widest) through 1000 (the device's longest zoom).
    Zoom,
    /// Pan in arc-seconds: 648000 is centred, 0 is 180 degrees one way, 1296000 the other.
    Pan,
    /// Tilt in arc-seconds, with the same scale as [`Control::Pan`].
    Tilt,
}

impl Control {
    /// Number of controls in the set.
    pub const COUNT: usize = 19;

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
        Control::Brightness,
        Control::Contrast,
        Control::Saturation,
        Control::Hue,
        Control::Sharpness,
        Control::Gamma,
        Control::BacklightCompensation,
        Control::PowerLineFrequency,
        Control::Zoom,
        Control::Pan,
        Control::Tilt,
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
            Control::Brightness
            | Control::Contrast
            | Control::Saturation
            | Control::Hue
            | Control::Sharpness => Spec {
                control: self,
                unit: "norm",
                range: Range::Manual { min: 0, max: 1_000 },
                default: ControlValue::Manual(500),
                auto: false,
            },
            Control::BacklightCompensation | Control::Zoom => Spec {
                control: self,
                unit: "norm",
                range: Range::Manual { min: 0, max: 1_000 },
                default: ControlValue::Manual(0),
                auto: false,
            },
            Control::Gamma => Spec {
                control: self,
                unit: "milli-gamma",
                range: Range::Manual {
                    min: 1_000,
                    max: 3_000,
                },
                default: ControlValue::Manual(2_200),
                auto: false,
            },
            Control::PowerLineFrequency => Spec {
                control: self,
                unit: "enum",
                range: Range::Manual { min: 0, max: 3 },
                default: ControlValue::Manual(0),
                auto: false,
            },
            Control::Pan | Control::Tilt => Spec {
                control: self,
                unit: "arcsec",
                range: Range::Manual {
                    min: 0,
                    max: 1_296_000,
                },
                default: ControlValue::Manual(648_000),
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
            Control::Brightness => "brightness",
            Control::Contrast => "contrast",
            Control::Saturation => "saturation",
            Control::Hue => "hue",
            Control::Sharpness => "sharpness",
            Control::Gamma => "gamma",
            Control::BacklightCompensation => "backlight_compensation",
            Control::PowerLineFrequency => "power_line_frequency",
            Control::Zoom => "zoom",
            Control::Pan => "pan",
            Control::Tilt => "tilt",
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

/// The power line frequency a [`Control::PowerLineFrequency`] value selects. The numbers are the
/// values the control takes, the same as the V4L2 menu's indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PowerLineFrequency {
    /// The filter is off.
    Disabled = 0,
    /// 50 Hz mains.
    Hz50 = 1,
    /// 60 Hz mains.
    Hz60 = 2,
    /// The device chooses.
    Auto = 3,
}

impl PowerLineFrequency {
    /// The control value that selects this frequency.
    pub const fn value(self) -> u32 {
        self as u32
    }

    /// The frequency a control value selects, or `None` past the range.
    pub const fn from_value(value: u32) -> Option<Self> {
        match value {
            0 => Some(PowerLineFrequency::Disabled),
            1 => Some(PowerLineFrequency::Hz50),
            2 => Some(PowerLineFrequency::Hz60),
            3 => Some(PowerLineFrequency::Auto),
            _ => None,
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
