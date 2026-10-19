//! What a native camera's 3A loop knew of a frame ([`super::NativeFrameMeta::aaa`]): the
//! white balance and colour the ISP applied, the scene's light, and the lens.

/// The 3A state of one frame of a sensor Styx drives (the exposure and gains are
/// [`super::NativeFrameMeta`]'s own fields).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NativeAaa {
    /// White balance gains (red, green, blue) the ISP applied to the frame.
    pub colour_gains: [f32; 3],
    /// Colour temperature AWB estimated, in kelvin (0: unknown).
    pub colour_temperature_k: f32,
    /// Scene brightness AE estimated, in lux (0: unknown).
    pub lux: f32,
    /// AE was locked (its exposure held).
    pub ae_locked: bool,
    /// AWB had converged.
    pub awb_converged: bool,
    /// The period of the light flicker AE detected, in nanoseconds (0: none).
    pub flicker_period_ns: u64,
    /// The focus lens, on cameras with one.
    pub lens: Option<NativeLens>,
}

/// A frame's focus lens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeLens {
    /// Where autofocus put the lens, in dioptres (0: infinity).
    pub position_dioptres: f32,
    /// What autofocus was doing.
    pub af_state: NativeAfState,
    /// Whether the lens had settled during the frame (`None`: unknown).
    pub settled: Option<bool>,
}

/// Autofocus state of a frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum NativeAfState {
    #[default]
    Idle,
    Scanning,
    Focused,
    Failed,
}

impl NativeAfState {
    /// A small number for recordings and the wire (0 idle, 1 scanning, 2 focused, 3 failed).
    pub fn tag(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Scanning => 1,
            Self::Focused => 2,
            Self::Failed => 3,
        }
    }

    /// [`Self::tag`]'s state; unknown tags read as idle.
    pub fn from_tag(tag: u8) -> Self {
        match tag {
            1 => Self::Scanning,
            2 => Self::Focused,
            3 => Self::Failed,
            _ => Self::Idle,
        }
    }
}
