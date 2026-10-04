//! AF controls, status and the lens request, shared with [`crate::Controls`],
//! [`crate::FrameMetadata`] and [`crate::Params`].

use serde::{Deserialize, Serialize};

/// What drives the lens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfMode {
    /// The application: [`crate::Controls::lens_position`] (the tuning's default position
    /// until one is given).
    #[default]
    Manual,
    /// One scan per trigger ([`crate::Controls::af_trigger`]), then the lens stays.
    Auto,
    /// Focus kept continuously: PDAF when the sensor sends phase data, else a contrast scan
    /// after each scene change.
    Continuous,
}

/// Which focus range a scan covers (the tuning's `ranges`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfRange {
    /// The normal range (infinity to about 8 cm on the IMX708 module).
    #[default]
    Normal,
    /// Close-up.
    Macro,
    /// Everything the lens can do.
    Full,
}

/// Scan and loop speed (the tuning's `speeds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfSpeed {
    /// Normal.
    #[default]
    Normal,
    /// Fast.
    Fast,
}

/// What AF reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfState {
    /// Not scanning: manual mode, or auto mode before a trigger (or after a cancel).
    #[default]
    Idle,
    /// A scan or the PDAF loop is moving the lens.
    Scanning,
    /// The last scan found a contrast peak (or PDAF is in focus).
    Focused,
    /// The last scan found no clear peak (flat or noisy contrast: low light, no texture) or
    /// PDAF ran into the end of the range.
    Failed,
}

/// A focus window: a rectangle as fractions of the output image (0..1), with a weight.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AfWindow {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
    /// Weight relative to the other windows (area-weighted within each).
    pub weight: f64,
}

impl Default for AfWindow {
    /// The default AF area: the middle half of the width, middle third of the height.
    fn default() -> Self {
        Self {
            x: 0.25,
            y: 1.0 / 3.0,
            width: 0.5,
            height: 1.0 / 3.0,
            weight: 1.0,
        }
    }
}

impl AfWindow {
    /// A window of weight 1.
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
            weight: 1.0,
        }
    }
}

/// Where the lens was for a frame, as the lens control reports it: predicted from the moves
/// written and the lens's move time model (VCMs have no position read-back).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LensState {
    /// Position in lens driver units (VCM code), at the middle of the frame's exposure.
    pub position: f64,
    /// The lens had settled at its last commanded position for the whole exposure.
    pub settled: bool,
}

/// A lens move: the driver position to be at from frame `frame` on (the lens control writes
/// it early enough for the move to settle, see [`crate::LensConfig::delay`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LensRequest {
    /// First frame the position should hold for.
    pub frame: u64,
    /// Lens driver position (VCM code).
    pub position: i32,
    /// The same in dioptres.
    pub dioptres: f64,
}

/// AF state, for applications.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AfStatus {
    /// The camera has a lens AF drives.
    pub active: bool,
    /// The mode in effect.
    pub mode: AfMode,
    /// What AF reports.
    pub state: AfState,
    /// The lens position AF commanded last, in dioptres.
    pub lens_position: Option<f64>,
    /// Contrast (focus figure of merit) in the AF windows, this frame.
    pub contrast: f64,
    /// PDAF phase in the windows (sensor units) and its confidence (0 without phase data).
    pub phase: f64,
    /// PDAF confidence.
    pub confidence: f64,
    /// Noise of the contrast: its frame-to-frame change with the lens still (filtered).
    pub contrast_noise: f64,
    /// Frame counter of the current scan (frames since it started), 0 when not scanning.
    pub scan_frames: u32,
}
