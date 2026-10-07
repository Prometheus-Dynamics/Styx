//! Multi-camera sync quality ([`SyncGroupMetrics`]): every live
//! [`FrameGrouper`](crate::multicam::FrameGrouper) of the process, listed in
//! [`snapshot`](super::snapshot) and exported as `styx_sync_*`.

use std::sync::{Mutex, Weak};

pub use styx_core::multicam::{CameraSyncReport, DropReason, SyncReport};

/// One frame grouper's sync quality.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct SyncGroupMetrics {
    /// The grouper's name (`FrameGrouper::named`), the `group` label.
    pub name: String,
    /// `strict`, `partial` or `latest`.
    pub policy: String,
    pub tolerance_ns: u64,
    /// The cameras' names, by index (the `camera` label).
    pub cameras: Vec<String>,
    pub report: SyncReport,
}

impl SyncGroupMetrics {
    /// `camera`'s name.
    pub fn camera_name(&self, camera: usize) -> &str {
        self.cameras.get(camera).map_or("", String::as_str)
    }
}

/// Something with a [`SyncGroupMetrics`] to show.
pub(crate) trait SyncSource: Send + Sync {
    fn sync_metrics(&self) -> SyncGroupMetrics;
}

static GROUPERS: Mutex<Vec<Weak<dyn SyncSource>>> = Mutex::new(Vec::new());

/// List `source` in [`snapshot`](super::snapshot) while it lives.
pub(crate) fn register(source: Weak<dyn SyncSource>) {
    if let Ok(mut all) = GROUPERS.lock() {
        all.retain(|w| w.strong_count() > 0);
        all.push(source);
    }
}

/// Every live grouper's metrics.
pub fn sync_groups() -> Vec<SyncGroupMetrics> {
    let sources: Vec<_> = GROUPERS
        .lock()
        .map(|all| all.iter().filter_map(Weak::upgrade).collect())
        .unwrap_or_default();
    sources.iter().map(|s| s.sync_metrics()).collect()
}

impl super::openmetrics::Text {
    pub(super) fn sync_group(&mut self, g: &SyncGroupMetrics) {
        use super::openmetrics::esc;
        let l = format!("group=\"{}\"", esc(&g.name));
        let r = &g.report;
        for (kind, v) in [
            ("complete", r.groups_complete),
            ("partial", r.groups_partial),
            ("stale", r.groups_stale),
        ] {
            self.counter(
                "styx_sync_groups_total",
                "Frame groups formed: complete, missing a camera, replaced before taken.",
                &format!("{l},kind=\"{kind}\""),
                v,
            );
        }
        self.gauge(
            "styx_sync_match_ratio",
            "Frames that went into groups over frames received.",
            &l,
            r.match_rate,
        );
        let ms = |ns: Option<u64>| ns.map(|ns| ns as f64 / 1e6);
        for (q, v) in [
            ("0.5", r.spread_p50_ns),
            ("0.99", r.spread_p99_ns),
            ("1", r.spread_max_ns),
        ] {
            self.gauge(
                "styx_sync_spread_ms",
                "Latest minus earliest timestamp in a group, over the last groups (quantile 1: max).",
                &format!("{l},quantile=\"{q}\""),
                ms(v),
            );
        }
        for c in &r.cameras {
            let cl = format!("{l},camera=\"{}\"", esc(g.camera_name(c.camera)));
            for (stage, v) in [("received", c.received), ("grouped", c.grouped)] {
                self.counter(
                    "styx_sync_frames_total",
                    "Frames a grouper received from a camera, and put in groups.",
                    &format!("{cl},stage=\"{stage}\""),
                    v,
                );
            }
            for reason in DropReason::ALL {
                self.counter(
                    "styx_sync_drops_total",
                    "Frames a grouper dropped, by reason.",
                    &format!("{cl},reason=\"{}\"", reason.as_str()),
                    c.drops_of(reason),
                );
            }
            self.gauge(
                "styx_sync_camera_connected",
                "1 while the camera feeds the grouper.",
                &cl,
                Some(f64::from(u8::from(c.connected))),
            );
            self.gauge(
                "styx_sync_frame_period_ms",
                "Frame period measured from the camera's timestamps.",
                &cl,
                ms(c.period_ns),
            );
            self.gauge(
                "styx_sync_offset_ms",
                "Camera timestamp minus the reference camera's, in the last group with both.",
                &cl,
                c.offset_ns.map(|ns| ns as f64 / 1e6),
            );
            self.gauge(
                "styx_sync_offset_mean_ms",
                "The offset averaged over the drift window.",
                &cl,
                c.offset_mean_ns.map(|ns| ns / 1e6),
            );
            self.gauge(
                "styx_sync_drift_ppm",
                "Drift of the camera's offset to the reference camera, parts per million.",
                &cl,
                c.drift_ppm,
            );
        }
    }
}
