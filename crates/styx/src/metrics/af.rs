//! Autofocus in a capture's metrics: the state, mode and lens of the latest frame and the scans
//! started, counted by the runtime (`styx_core::metrics::AaaCounters`), named here.

#[cfg(any(test, feature = "native"))]
pub(crate) use styx_core::metrics::AfSample;
pub(crate) use styx_core::metrics::{AfModeKind, AfReading, AfStateKind};

/// Fills the AF fields of `out` from what the runtime read back.
pub(crate) fn fill(af: &AfReading, out: &mut super::camera::AaaState) {
    out.af_state = Some(
        match af.state {
            AfStateKind::Idle => "idle",
            AfStateKind::Scanning => "scanning",
            AfStateKind::Focused => "focused",
            AfStateKind::Failed => "failed",
        }
        .into(),
    );
    out.af_mode = Some(
        match af.mode {
            AfModeKind::Manual => "manual",
            AfModeKind::Auto => "auto",
            AfModeKind::Continuous => "continuous",
        }
        .into(),
    );
    out.lens_position_dioptres = af.lens_dioptres;
    out.lens_settled = af.lens_settled;
    out.af_scans = Some(af.scans);
}
