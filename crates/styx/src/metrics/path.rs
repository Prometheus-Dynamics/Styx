//! Snapshots of the frame path's own costs: the time between consecutive hops of the frames a
//! capture or a camera service client saw ([`HopMetrics`], from
//! `styx_core::metrics::HopCounters`), and the process's copies by site, dma-buf syncs and
//! exhausted pools ([`PathMetrics`]). See `docs/metrics.md`.

use std::fmt::Write as _;

use styx_core::metrics::{CopySite, HopCounters, TrafficStage, path_counters, traffic_counters};

use super::camera::Window;
use super::live::RingWindow as _;

/// The time from one hop to the next over the last frames.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct HopWindow {
    /// The hop before (`sensor`, `dequeued`, `isp_done`, `queued`, `taken`, `sent`,
    /// `received`; see `styx_core::buffer::Hop`).
    pub from: String,
    /// The hop.
    pub to: String,
    pub window: Window,
}

/// Hop times and copies of the frames recorded at one place (a capture's consumers taking
/// them, a camera service client sending them, a consumer process importing them).
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct HopMetrics {
    /// Frames recorded.
    pub frames: u64,
    /// Of those, frames that reached here without a copy of their pixels.
    pub zero_copy: u64,
    /// Frames copied on the way (conversions, crops into new memory, memfd copies).
    pub copied: u64,
    /// Copies (a frame may be copied more than once) and their bytes.
    pub copies: u64,
    pub copied_bytes: u64,
    /// Each hop from the one before it, in path order (only hops frames passed).
    pub hops: Vec<HopWindow>,
    /// First to last recorded hop (sensor to here when the sensor timestamp is known).
    pub total: Window,
}

impl HopMetrics {
    pub(crate) fn of(c: &HopCounters) -> Self {
        let frames = c.frames.get();
        let copied = c.copied_frames.get();
        Self {
            frames,
            zero_copy: frames.saturating_sub(copied),
            copied,
            copies: c.copies.get(),
            copied_bytes: c.copied_bytes.get(),
            hops: c
                .windows()
                .map(|(from, to, ring)| HopWindow {
                    from: from.name().into(),
                    to: to.name().into(),
                    window: ring.window(),
                })
                .collect(),
            total: c.total.window(),
        }
    }

    /// The window into the hop named `to`.
    pub fn hop(&self, to: &str) -> Option<&HopWindow> {
        self.hops.iter().find(|h| h.to == to)
    }

    /// Whether nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }
}

/// Copies made at one place, process-wide.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct CopySiteMetrics {
    /// `softisp`, `conversion`, `region`, `memfd_export`, `materialize`, `capture`, `raw`,
    /// `other`.
    pub site: String,
    pub copies: u64,
    pub bytes: u64,
}

/// The memory one stage read and wrote, process-wide: totals since the start, so per frame is
/// a difference of bytes over a difference of jobs, and MB/s a difference over time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct StageTrafficMetrics {
    /// `receiver`, `isp`, `isp_pass`, `cpu_isp`, `cpu_pyramid`, `cpu_copy`, `gpu`, `cpu_scale`.
    pub stage: String,
    /// `hardware`, `cpu` or `gpu`.
    pub unit: String,
    /// Jobs (a frame, a pass, a level, a copy).
    pub jobs: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
}

/// The process's frame path costs: copies by site, dma-buf cache maintenance, pools that had
/// no free buffer, memory traffic by stage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "metrics-serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct PathMetrics {
    /// Every copy site (zeros included).
    pub copies: Vec<CopySiteMetrics>,
    /// `DMA_BUF_IOCTL_SYNC` calls (a start and an end each count) and their time.
    pub dmabuf_syncs: u64,
    pub dmabuf_sync_ns: u64,
    /// Times a pool or a capture found every buffer held (the ISP dropping a frame, a shared
    /// memfd pool allocating another buffer).
    pub pool_exhausted: u64,
    /// Memory read and written by stage (zeros included).
    pub traffic: Vec<StageTrafficMetrics>,
    /// Where the traffic comes from: `computed` (from the buffers each job touches; no bus
    /// counters are read yet).
    pub traffic_source: String,
}

impl PathMetrics {
    /// All copies and their bytes.
    pub fn total_copies(&self) -> (u64, u64) {
        self.copies
            .iter()
            .fold((0, 0), |(c, b), s| (c + s.copies, b + s.bytes))
    }
}

/// The process's copies, syncs and exhausted pools now.
pub fn path() -> PathMetrics {
    let p = path_counters();
    #[allow(unused_mut)]
    let (mut syncs, mut sync_ns) = (p.syncs.get(), p.sync_ns.get());
    #[cfg(feature = "native")]
    {
        let (n, ns) = styx_native::dma::sync_stats();
        syncs += n;
        sync_ns += ns;
    }
    PathMetrics {
        copies: CopySite::ALL
            .into_iter()
            .map(|site| CopySiteMetrics {
                site: site.name().into(),
                copies: p.copies(site),
                bytes: p.copied_bytes(site),
            })
            .collect(),
        dmabuf_syncs: syncs,
        dmabuf_sync_ns: sync_ns,
        pool_exhausted: p.pool_exhausted.get(),
        traffic: TrafficStage::ALL
            .into_iter()
            .map(|stage| {
                let t = traffic_counters();
                StageTrafficMetrics {
                    stage: stage.name().into(),
                    unit: stage.unit().into(),
                    jobs: t.jobs(stage),
                    read_bytes: t.read(stage),
                    written_bytes: t.written(stage),
                }
            })
            .collect(),
        traffic_source: "computed".into(),
    }
}

impl super::openmetrics::Text {
    /// `hops` as `styx_<what>_hop_ms{...,from,to,quantile}` windows and copy counters.
    pub(super) fn hops(&mut self, what: HopMetricNames, labels: &str, h: &HopMetrics) {
        if h.is_empty() {
            return;
        }
        for w in &h.hops {
            self.window(
                what.hop,
                "Time from one hop of a frame to the next over the last frames (quantile 1: max).",
                &format!("{labels},from=\"{}\",to=\"{}\"", w.from, w.to),
                &w.window,
            );
        }
        self.window(
            what.total,
            "First to last recorded hop of a frame over the last frames (quantile 1: max).",
            labels,
            &h.total,
        );
        for (kind, v) in [("zero_copy", h.zero_copy), ("copied", h.copied)] {
            self.counter(
                what.frames,
                "Frames recorded, zero-copy or copied on the way.",
                &format!("{labels},kind=\"{kind}\""),
                v,
            );
        }
        self.counter(
            what.copied_bytes,
            "Bytes of the copies made of the frames recorded.",
            labels,
            h.copied_bytes,
        );
    }

    pub(super) fn path(&mut self, p: &PathMetrics) {
        for t in &p.traffic {
            let l = format!("stage=\"{}\",unit=\"{}\"", t.stage, t.unit);
            self.counter(
                "styx_process_traffic_jobs_total",
                "Jobs of a stage that moves frame memory (frames, passes, levels, copies).",
                &l,
                t.jobs,
            );
            for (dir, v) in [("read", t.read_bytes), ("written", t.written_bytes)] {
                self.counter(
                    "styx_process_traffic_bytes_total",
                    "Frame memory a stage read or wrote (computed from the buffers it touches).",
                    &format!("{l},direction=\"{dir}\""),
                    v,
                );
            }
        }
        for s in &p.copies {
            let l = format!("site=\"{}\"", s.site);
            self.counter(
                "styx_process_copies_total",
                "Copies of frame pixels in the process, by site.",
                &l,
                s.copies,
            );
            self.counter(
                "styx_process_copied_bytes_total",
                "Bytes of frame pixels copied in the process, by site.",
                &l,
                s.bytes,
            );
        }
        self.counter(
            "styx_process_dmabuf_syncs_total",
            "DMA_BUF_IOCTL_SYNC calls (cache maintenance) in the process.",
            "",
            p.dmabuf_syncs,
        );
        self.sample(
            "styx_process_dmabuf_sync_seconds_total",
            "counter",
            "Time spent in DMA_BUF_IOCTL_SYNC calls.",
            "",
            p.dmabuf_sync_ns as f64 / 1e9,
        );
        self.counter(
            "styx_process_pool_exhausted_total",
            "Times a pool or a capture found every buffer held.",
            "",
            p.pool_exhausted,
        );
    }
}

/// Metric names of one place's [`HopMetrics`].
#[derive(Clone, Copy)]
pub(super) struct HopMetricNames {
    pub(super) hop: &'static str,
    pub(super) total: &'static str,
    pub(super) frames: &'static str,
    pub(super) copied_bytes: &'static str,
}

pub(super) const CAMERA_HOPS: HopMetricNames = HopMetricNames {
    hop: "styx_camera_hop_ms",
    total: "styx_camera_path_ms",
    frames: "styx_camera_path_frames_total",
    copied_bytes: "styx_camera_path_copied_bytes_total",
};

pub(super) const CONSUMER_HOPS: HopMetricNames = HopMetricNames {
    hop: "styx_consumer_hop_ms",
    total: "styx_consumer_path_ms",
    frames: "styx_consumer_path_frames_total",
    copied_bytes: "styx_consumer_path_copied_bytes_total",
};

/// A [`HopMetrics`] as a few lines of text, for tools (`metrics_top`).
pub fn hop_lines(h: &HopMetrics) -> String {
    let mut out = String::new();
    let ms = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |v| format!("{v:.3}"));
    for w in &h.hops {
        let _ = writeln!(
            out,
            "    {:>9} -> {:<9} p50 {:>8} p99 {:>8} max {:>8} ms",
            w.from,
            w.to,
            ms(w.window.p50_ms),
            ms(w.window.p99_ms),
            ms(w.window.max_ms),
        );
    }
    if !h.hops.is_empty() {
        let _ = writeln!(
            out,
            "    {:>22} p50 {:>8} p99 {:>8} max {:>8} ms   {} frames, {} zero-copy, {} copied ({} B)",
            "total",
            ms(h.total.p50_ms),
            ms(h.total.p99_ms),
            ms(h.total.max_ms),
            h.frames,
            h.zero_copy,
            h.copied,
            h.copied_bytes,
        );
    }
    out
}
