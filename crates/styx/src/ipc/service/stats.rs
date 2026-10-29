//! A camera service's statistics and metrics replies.

use std::sync::atomic::Ordering;

use super::{CameraServiceStats, Service};

impl Service {
    pub(super) fn stats(&self) -> CameraServiceStats {
        let counters = &self.counters;
        CameraServiceStats {
            clients: self.clients(),
            rejected: counters.rejected.load(Ordering::Relaxed),
            unauthorized: counters.unauthorized.load(Ordering::Relaxed),
            restarts: counters.restarts.load(Ordering::Relaxed),
            sent: counters.sent.load(Ordering::Relaxed),
            copied: counters.copied.load(Ordering::Relaxed),
            skipped: counters.skipped.load(Ordering::Relaxed),
            revoked: counters.revoked.load(Ordering::Relaxed),
        }
    }

    pub(super) fn metrics(&self) -> crate::metrics::ServiceMetrics {
        let stats = self.stats();
        let owned: Vec<crate::metrics::CaptureMetrics> = self
            .cameras
            .lock()
            .iter()
            .flat_map(|c| c.captures())
            .collect();
        crate::metrics::ServiceMetrics {
            clients: stats.clients,
            rejected: stats.rejected,
            unauthorized: stats.unauthorized,
            restarts: stats.restarts,
            sent: stats.sent,
            copied: stats.copied,
            skipped: stats.skipped,
            revoked: stats.revoked,
            client_metrics: self.client_metrics.snapshot(),
            last_error: self.counters.last_error.lock().clone(),
            // Only this service's captures: the process runs other services' too.
            snapshot: crate::metrics::snapshot_of(&owned),
        }
    }
}
