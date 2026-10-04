//! AF tuning: focus ranges, scan and loop speeds, PDAF confidence, the dioptre ↔ lens driver
//! map. Names and defaults follow Raspberry Pi's `rpi.af` (defaults: IMX708 in a Camera
//! Module 3 with the standard lens), see [`super`].

use alloc::{format, string::String};

use serde::{Deserialize, Serialize};

use crate::error::{AlgoError, Result};
use crate::pwl::Pwl;

/// A focus range in dioptres (1 / metres; 0 is infinity).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AfRangeTuning {
    /// Far limit.
    pub min: f64,
    /// Near limit.
    pub max: f64,
    /// Default position (the hyperfocal position for the normal range).
    pub default: f64,
}

impl Default for AfRangeTuning {
    fn default() -> Self {
        Self {
            min: 0.0,
            max: 12.0,
            default: 1.0,
        }
    }
}

/// The ranges by name (`normal`, `macro`, `full`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AfRanges {
    /// The normal range.
    pub normal: AfRangeTuning,
    /// Close-up range (default: the normal range).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#macro: Option<AfRangeTuning>,
    /// Everything (default: the union of normal and macro, the normal default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full: Option<AfRangeTuning>,
}

impl AfRanges {
    /// The range for an [`super::AfRange`].
    pub fn get(&self, r: super::AfRange) -> AfRangeTuning {
        let macro_ = self.r#macro.unwrap_or(self.normal);
        match r {
            super::AfRange::Normal => self.normal,
            super::AfRange::Macro => macro_,
            super::AfRange::Full => self.full.unwrap_or(AfRangeTuning {
                min: self.normal.min.min(macro_.min),
                max: self.normal.max.max(macro_.max),
                default: self.normal.default,
            }),
        }
    }
}

/// Speed-dependent parameters (gains and delays count frames of the algorithm's rate).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AfSpeedTuning {
    /// Coarse scan step in dioptres.
    pub step_coarse: f64,
    /// Fine scan step in dioptres (0: no fine scan).
    pub step_fine: f64,
    /// A scan stops once contrast falls below this fraction of its peak; focus is reported
    /// found when the peak stands out by this ratio.
    pub contrast_ratio: f64,
    /// Continuous AF: contrast or colour changing by more than this ratio is a scene change.
    pub retrigger_ratio: f64,
    /// Continuous AF: frames of stability after a scene change before a new scan.
    pub retrigger_delay: u32,
    /// PDAF loop gain: dioptres per phase unit (negative for IMX708's sign).
    pub pdaf_gain: f64,
    /// Continuous PDAF: moves below this (dioptres) are squashed (cubically) against wobble.
    pub pdaf_squelch: f64,
    /// Largest lens move per frame in dioptres.
    pub max_slew: f64,
    /// Triggered PDAF: frames the loop runs.
    pub pdaf_frames: u32,
    /// Frames of low PDAF confidence before falling back to a contrast scan (0: no PDAF).
    pub dropout_frames: u32,
    /// Frames between scan steps when the lens's position is not reported per frame (the
    /// contrast of a step is then taken this many frames after the move).
    pub step_frames: u32,
}

impl Default for AfSpeedTuning {
    fn default() -> Self {
        Self {
            step_coarse: 1.0,
            step_fine: 0.25,
            contrast_ratio: 0.75,
            retrigger_ratio: 0.75,
            retrigger_delay: 10,
            pdaf_gain: -0.02,
            pdaf_squelch: 0.125,
            max_slew: 2.0,
            pdaf_frames: 20,
            dropout_frames: 6,
            step_frames: 4,
        }
    }
}

/// The speeds by name (`normal`, `fast`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AfSpeeds {
    /// Normal.
    pub normal: AfSpeedTuning,
    /// Fast (default: normal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<AfSpeedTuning>,
}

impl AfSpeeds {
    /// The parameters for an [`super::AfSpeed`].
    pub fn get(&self, s: super::AfSpeed) -> AfSpeedTuning {
        match s {
            super::AfSpeed::Normal => self.normal,
            super::AfSpeed::Fast => self.fast.unwrap_or(self.normal),
        }
    }
}

/// The `[af]` tuning section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AfTuning {
    /// Focus ranges.
    pub ranges: AfRanges,
    /// Speeds.
    pub speeds: AfSpeeds,
    /// PDAF: confidence that counts as data (hysteresis and the continuous loop's softening).
    pub conf_epsilon: f64,
    /// PDAF: least confidence of a cell that is used.
    pub conf_thresh: f64,
    /// PDAF: confidence of a cell is clipped to this.
    pub conf_clip: f64,
    /// Frames left out at the start and after a mode change.
    pub skip_frames: u32,
    /// PDAF is not trusted while the light looks infrared (R ≈ G ≈ B over most of the image).
    pub check_for_ir: bool,
    /// Dioptres → lens driver position (VCM code). Empty: the lens's own map
    /// ([`crate::LensConfig::map`]), else a straight line over the lens's range for the normal
    /// focus range.
    #[serde(skip_serializing_if = "Pwl::is_empty")]
    pub map: Pwl,
    /// Styx: a frame whose lens position is reported is measured as soon as the lens has
    /// settled at the scan step, instead of after `step_frames` (true by default).
    pub frame_exact: bool,
}

impl Default for AfTuning {
    fn default() -> Self {
        Self {
            ranges: AfRanges::default(),
            speeds: AfSpeeds::default(),
            conf_epsilon: 8.0,
            conf_thresh: 16.0,
            conf_clip: 512.0,
            skip_frames: 5,
            check_for_ir: false,
            map: Pwl::default(),
            frame_exact: true,
        }
    }
}

impl AfTuning {
    /// Check the values.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(AlgoError::Tuning(format!("af: {m}")));
        for r in [
            super::AfRange::Normal,
            super::AfRange::Macro,
            super::AfRange::Full,
        ] {
            let t = self.ranges.get(r);
            if !(t.min.is_finite() && t.max.is_finite() && t.min <= t.max) {
                return bad(format!("range {r:?}: min must not exceed max"));
            }
            if !(t.min..=t.max).contains(&t.default) {
                return bad(format!("range {r:?}: default outside min..max"));
            }
        }
        for s in [super::AfSpeed::Normal, super::AfSpeed::Fast] {
            let t = self.speeds.get(s);
            if !(t.step_coarse > 0.0 && t.step_fine >= 0.0 && t.max_slew > 0.0) {
                return bad(format!("speed {s:?}: steps and max_slew must be positive"));
            }
            let unit = |v: f64| v > 0.0 && v < 1.0;
            if !(unit(t.contrast_ratio) && unit(t.retrigger_ratio)) {
                return bad(format!("speed {s:?}: ratios must be in (0, 1)"));
            }
            if !t.pdaf_gain.is_finite() || t.pdaf_squelch < 0.0 {
                return bad(format!("speed {s:?}: pdaf_gain / pdaf_squelch"));
            }
        }
        if !(self.conf_thresh >= 0.0 && self.conf_clip >= self.conf_thresh) {
            return bad("conf_clip must be at least conf_thresh".into());
        }
        if !self.map.is_empty() {
            let pts = self.map.points();
            if pts.len() < 2 || pts.windows(2).any(|w| w[1].1 == w[0].1) {
                return bad("map must be strictly monotonic with two points or more".into());
            }
        }
        Ok(())
    }
}
