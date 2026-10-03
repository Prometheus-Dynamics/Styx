//! Autofocus in a capture's metrics: the state, mode and lens of the latest frame, recorded with
//! relaxed atomics as the 3A loop reports them, and the scans started so far.

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

/// AF states as the loop reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub(crate) enum AfStateKind {
    Idle = 1,
    Scanning = 2,
    Focused = 3,
    Failed = 4,
}

/// AF modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub(crate) enum AfModeKind {
    Manual = 1,
    Auto = 2,
    Continuous = 3,
}

/// What AF did for one frame (cameras with a focus lens).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AfSample {
    pub state: AfStateKind,
    pub mode: AfModeKind,
    /// The lens position AF commanded, in dioptres.
    pub lens_dioptres: Option<f64>,
    /// The lens had settled at its commanded position for the whole exposure (`None`: not
    /// reported for this frame).
    pub lens_settled: Option<bool>,
}

/// The latest [`AfSample`] and the scans counted.
pub(crate) struct AfLive {
    /// 0: never reported; else an [`AfStateKind`].
    state: AtomicU8,
    mode: AtomicU8,
    /// `f32` bits; NaN without a position.
    lens: AtomicU32,
    /// 0 unknown, 1 moving, 2 settled.
    settled: AtomicU8,
    scans: AtomicU64,
}

impl Default for AfLive {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(0),
            mode: AtomicU8::new(0),
            lens: AtomicU32::new(f32::NAN.to_bits()),
            settled: AtomicU8::new(0),
            scans: AtomicU64::new(0),
        }
    }
}

impl AfLive {
    #[inline]
    pub(crate) fn record(&self, s: &AfSample) {
        let previous = self.state.swap(s.state as u8, Relaxed);
        // A scan starts when AF enters scanning (a continuous scan, or each trigger).
        if s.state == AfStateKind::Scanning && previous != AfStateKind::Scanning as u8 {
            self.scans.fetch_add(1, Relaxed);
        }
        self.mode.store(s.mode as u8, Relaxed);
        let lens = s.lens_dioptres.map_or(f32::NAN, |d| d as f32);
        self.lens.store(lens.to_bits(), Relaxed);
        let settled = s.lens_settled.map_or(0, |s| 1 + u8::from(s));
        self.settled.store(settled, Relaxed);
    }

    /// Fills the AF fields of `out`; `false` when AF never reported.
    pub(crate) fn read(&self, out: &mut super::camera::AaaState) -> bool {
        let state = self.state.load(Relaxed);
        if state == 0 {
            return false;
        }
        out.af_state = Some(
            match state {
                1 => "idle",
                2 => "scanning",
                3 => "focused",
                _ => "failed",
            }
            .into(),
        );
        out.af_mode = Some(
            match self.mode.load(Relaxed) {
                1 => "manual",
                2 => "auto",
                _ => "continuous",
            }
            .into(),
        );
        let lens = f32::from_bits(self.lens.load(Relaxed));
        out.lens_position_dioptres = (!lens.is_nan()).then_some(f64::from(lens));
        out.lens_settled = match self.settled.load(Relaxed) {
            0 => None,
            s => Some(s == 2),
        };
        out.af_scans = Some(self.scans.load(Relaxed));
        true
    }
}
