//! The run's numbers: the JSON the tool writes (`--report`) and the short table it prints. Every
//! figure is from this process's clocks and counters, the service's status and `/proc`; a figure
//! the board does not report is written as `null` with its reason next to it.

use crate::clock::Memory;
use crate::hist::Histogram;
use crate::json::Json;
use crate::stats::{FrameStats, IspStats};

/// A camera's counts as the service's `list` line gave them, at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCamera {
    pub id: String,
    pub state: String,
    pub frames: Option<u64>,
    pub skipped: Option<u64>,
    /// The Lemnos stream reader's own skipped count (`lemnos_skipped`), apart from `skipped`.
    pub lemnos_skipped: Option<u64>,
    pub last_id: Option<u64>,
    pub publish: String,
    pub readers: Option<u64>,
    pub published: Option<u64>,
    pub publish_dropped: Option<u64>,
    pub health: String,
    /// The rate asked for, in milli-fps (`None` for `none` in the status).
    pub requested_milli: Option<u64>,
    /// The rate the backend reports, in milli-fps (`None` for `unknown`).
    pub achieved_milli: Option<u64>,
    /// The rate in the last frame's metadata, in milli-fps (`None` for none).
    pub achieved_meta_milli: Option<u64>,
    /// The rate measured by the service from the last frames' capture timestamps.
    pub measured_milli: Option<u64>,
    pub rate_clamped: Option<bool>,
    /// The service's one-line reason the achieved rate differs from the requested, if it does.
    pub rate_reason: Option<String>,
    pub keep: Option<u64>,
    pub buffers: Option<u64>,
    pub hardware_reserve: Option<u64>,
    pub reclaimed: Option<u64>,
    pub raw: String,
}

/// Copies at one site in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySite {
    pub site: String,
    pub copies: usize,
    pub bytes: usize,
}

/// CPU time and memory of the two processes over the run.
#[derive(Debug, Clone, Default)]
pub struct Process {
    pub cpu_ns: Option<u64>,
    pub memory_start: Option<Memory>,
    pub memory_end: Option<Memory>,
}

/// The whole run.
#[derive(Debug, Clone)]
pub struct Report {
    pub socket: String,
    pub control: Option<String>,
    pub service_pid: Option<u32>,
    pub stop: String,
    pub wall_s: f64,
    pub asked_frames: Option<u64>,
    pub asked_seconds: Option<f64>,
    /// The asked rate, in thousandths of a frame per second, when the operator gave it.
    pub asked_fps_milli: Option<u32>,
    pub frames: FrameStats,
    /// Reader counts for this run (ring skips and so on): end minus start.
    pub skipped: u64,
    /// Producer stall edges and resume edges seen in this run, and how many resumes were lost.
    pub stalls: u64,
    pub resumes: u64,
    pub lost_resumes: u64,
    /// The time the camera spent stalled in this run, in ns (the edges' times).
    pub stall_ns_total: u64,
    pub superseded: u64,
    pub limited: u64,
    pub malformed: u64,
    pub dma_sync_errors: u64,
    pub held_at_end: u32,
    pub service_start: Option<ServiceCamera>,
    pub service_end: Option<ServiceCamera>,
    pub copies: Vec<CopySite>,
    /// The service's own copy counters, from its status (its process, not this one).
    pub service_copies: Vec<CopySite>,
    /// Where the requested rate came from: the operator's `--asked-fps`, the status, or none.
    pub requested_source: &'static str,
    /// The rate the reader's last frame metadata reports, in milli-fps.
    pub achieved_metadata_milli: Option<u32>,
    pub own: Process,
    pub service: Process,
    pub isp: Option<IspStats>,
    pub last_metadata: Option<String>,
}

fn hist_json(h: &Histogram) -> Json {
    Json::Object(vec![
        ("samples", Json::Int(h.len())),
        ("min", Json::opt_int(h.min())),
        ("median", Json::opt_int(h.median())),
        ("p99", Json::opt_int(h.quantile(0.99))),
        ("max", Json::opt_int(h.max())),
    ])
}

fn memory_json(m: Option<Memory>) -> Json {
    match m {
        Some(m) => Json::Object(vec![
            ("rss_bytes", Json::Int(m.rss_bytes)),
            ("hwm_bytes", Json::Int(m.peak_bytes)),
        ]),
        None => Json::Null,
    }
}

fn camera_json(c: &Option<ServiceCamera>) -> Json {
    match c {
        None => Json::Null,
        Some(c) => Json::Object(vec![
            ("id", Json::Str(c.id.clone())),
            ("state", Json::Str(c.state.clone())),
            ("frames", Json::opt_int(c.frames)),
            ("skipped", Json::opt_int(c.skipped)),
            ("lemnos_skipped", Json::opt_int(c.lemnos_skipped)),
            ("last_id", Json::opt_int(c.last_id)),
            ("publish", Json::Str(c.publish.clone())),
            ("readers", Json::opt_int(c.readers)),
            ("published", Json::opt_int(c.published)),
            ("publish_dropped", Json::opt_int(c.publish_dropped)),
            ("health", Json::Str(c.health.clone())),
            ("requested_fps_milli", Json::opt_int(c.requested_milli)),
            ("achieved_fps_milli", Json::opt_int(c.achieved_milli)),
            (
                "achieved_metadata_fps_milli",
                Json::opt_int(c.achieved_meta_milli),
            ),
            ("measured_fps_milli", Json::opt_int(c.measured_milli)),
            (
                "rate_clamped",
                c.rate_clamped.map_or(Json::Null, Json::Bool),
            ),
            ("rate_reason", Json::opt_str(c.rate_reason.as_deref())),
            ("keep", Json::opt_int(c.keep)),
            ("buffers", Json::opt_int(c.buffers)),
            ("hardware_reserve", Json::opt_int(c.hardware_reserve)),
            ("reclaimed", Json::opt_int(c.reclaimed)),
        ]),
    }
}

fn milli_json(milli: Option<u64>) -> Json {
    Json::opt_float(milli.map(|m| m as f64 / 1000.0))
}

impl Report {
    /// Frames per second the sensor achieved, as the metadata or the status reports it. Neither
    /// carries a rate at this commit, so this is always absent.
    pub const SENSOR_RATE: Option<f64> = None;
    pub const SENSOR_RATE_SOURCE: &'static str =
        "not reported: the frame metadata and the status line carry no rate";

    fn ns_per_frame(total: Option<u64>, frames: u64) -> Option<f64> {
        let total = total?;
        (frames > 0).then(|| total as f64 / frames as f64)
    }

    /// The JSON of the run. Every field is present; absent figures are `null`.
    pub fn to_json(&self) -> Json {
        let f = &self.frames;
        let delivered = f.delivered;
        let mean = f.mean_interval_ns();
        let service_frames_delta = match (&self.service_start, &self.service_end) {
            (Some(a), Some(b)) => match (a.frames, b.frames) {
                (Some(x), Some(y)) => Some(y.saturating_sub(x)),
                _ => None,
            },
            _ => None,
        };
        let service_ns_per_camera_frame = match (self.service.cpu_ns, service_frames_delta) {
            (Some(ns), Some(frames)) if frames > 0 => Some(ns as f64 / frames as f64),
            _ => None,
        };
        let copies_total: usize = self.copies.iter().map(|c| c.copies).sum();
        Json::Object(vec![
            ("tool", Json::Str(env!("CARGO_PKG_NAME").into())),
            ("socket", Json::Str(self.socket.clone())),
            ("control", Json::opt_str(self.control.as_deref())),
            ("stop", Json::Str(self.stop.clone())),
            ("wall_s", Json::Float(self.wall_s)),
            ("asked", Json::Object(vec![
                ("frames", Json::opt_int(self.asked_frames)),
                ("seconds", Json::opt_float(self.asked_seconds)),
                ("fps_milli", Json::opt_int(self.asked_fps_milli.map(u64::from))),
                ("fps_source", Json::Str(if self.asked_fps_milli.is_some() {
                    "operator (--asked-fps); the status line carries no requested rate".into()
                } else {
                    "not given".into()
                })),
            ])),
            ("frames", Json::Object(vec![
                ("delivered", Json::Int(delivered)),
                ("skipped_by_ring", Json::Int(self.skipped)),
                ("stalls", Json::Int(self.stalls)),
                ("stall_ms_total", Json::Float(self.stall_ns_total as f64 / 1e6)),
                ("stall_gaps_ms", Json::Array(f.stall_gaps_ns.iter().map(|ns| Json::Float(*ns as f64 / 1e6)).collect())),
                ("discontinuities", Json::Int(self.lost_resumes)),
                ("producer_stalls", Json::Int(self.stalls)),
                ("producer_resumes", Json::Int(self.resumes)),
                ("producer_resumes_lost", Json::Int(self.lost_resumes)),
                ("superseded", Json::Int(self.superseded)),
                ("limited", Json::Int(self.limited)),
                ("malformed", Json::Int(self.malformed)),
                ("gaps", Json::Int(f.gaps)),
                ("largest_gap", Json::Int(f.largest_gap)),
                ("duplicates", Json::Int(f.duplicates)),
                ("out_of_order", Json::Int(f.out_of_order)),
                ("metadata_refused", Json::Int(f.metadata_refused)),
                ("first_id", Json::opt_int(f.first_id)),
                ("last_id", Json::opt_int(f.last_id)),
                ("dma_sync_errors", Json::Int(self.dma_sync_errors)),
                ("held_at_end", Json::Int(u64::from(self.held_at_end))),
                ("last_metadata", Json::opt_str(self.last_metadata.as_deref())),
            ])),
            ("rate", Json::Object(vec![
                ("measured_fps", Json::opt_float(f.measured_fps())),
                ("mean_interval_ns", Json::opt_float(mean)),
                ("interval_min_ns", Json::opt_int(f.intervals.min())),
                ("interval_median_ns", Json::opt_int(f.intervals.median())),
                ("interval_max_ns", Json::opt_int(f.intervals.max())),
                ("interval_p99_ns", Json::opt_int(f.intervals.quantile(0.99))),
                ("asked_fps", Json::opt_float(self.asked_fps_milli.map(|m| f64::from(m) / 1000.0))),
                ("requested_fps", milli_json(self.requested_milli().map(u64::from))),
                ("requested_source", Json::Str(self.requested_source.into())),
                ("achieved_backend_fps", milli_json(self.achieved_milli().map(u64::from))),
                ("achieved_metadata_fps", milli_json(self.achieved_metadata_milli.map(u64::from))),
                ("measured_service_fps", milli_json(self.service_measured_milli().map(u64::from))),
                ("measured_reader_fps", Json::opt_float(f.measured_fps())),
                ("rate_reason", Json::opt_str(self.rate_reason().as_deref())),
                ("sensor_fps", Json::opt_float(Self::SENSOR_RATE)),
                ("sensor_fps_source", Json::Str(Self::SENSOR_RATE_SOURCE.into())),
                ("service_status_frames_delta", Json::opt_int(service_frames_delta)),
            ])),
            ("latency_ns", Json::Object(vec![
                ("min", Json::opt_int(f.latency.min())),
                ("median", Json::opt_int(f.latency.median())),
                ("p99", Json::opt_int(f.latency.quantile(0.99))),
                ("max", Json::opt_int(f.latency.max())),
                ("samples", Json::Int(f.latency.len())),
                ("negative_samples", Json::Int(f.negative_latency)),
                ("note", Json::Str("median and p99 are bucket midpoints (within 3 %); min and max are exact".into())),
            ])),
            ("copies", Json::Object(vec![
                ("this_process_total", Json::Int(copies_total as u64)),
                ("sites", Json::Array(self.copies.iter().map(|c| Json::Object(vec![
                    ("site", Json::Str(c.site.clone())),
                    ("copies", Json::Int(c.copies as u64)),
                    ("bytes", Json::Int(c.bytes as u64)),
                ])).collect())),
                ("service_total", Json::Int(self.service_copies.iter().map(|c| c.copies as u64).sum())),
                ("service", Json::Array(self.service_copies.iter().map(|c| Json::Object(vec![
                    ("site", Json::Str(c.site.clone())),
                    ("copies", Json::Int(c.copies as u64)),
                    ("bytes", Json::Int(c.bytes as u64)),
                ])).collect())),
            ])),
            ("cpu", Json::Object(vec![
                ("reader_total_ns", Json::opt_int(self.own.cpu_ns)),
                ("reader_ns_per_frame", Json::opt_float(Self::ns_per_frame(self.own.cpu_ns, delivered))),
                ("service_pid", Json::opt_int(self.service_pid.map(u64::from))),
                ("service_total_ns", Json::opt_int(self.service.cpu_ns)),
                ("service_ns_per_frame", Json::opt_float(service_ns_per_camera_frame)),
                ("service_ns_per_frame_note", Json::Str("service total CPU over this camera's frames from the status (the service also serves its other cameras)".into())),
            ])),
            ("memory", Json::Object(vec![
                ("reader_start", memory_json(self.own.memory_start)),
                ("reader_end", memory_json(self.own.memory_end)),
                ("service_start", memory_json(self.service.memory_start)),
                ("service_end", memory_json(self.service.memory_end)),
            ])),
            ("isp", match &self.isp {
                None => Json::Null,
                Some(i) => Json::Object(vec![
                    ("output", Json::Str(i.output.clone())),
                    ("width", Json::Int(u64::from(i.width))),
                    ("height", Json::Int(u64::from(i.height))),
                    ("frames", Json::Int(i.times.len())),
                    ("errors", Json::Int(i.errors)),
                    ("time_ns", hist_json(&i.times)),
                    ("pool", Json::Object(vec![
                        ("buffers", Json::Int(i.pool_buffers as u64)),
                        ("acquired", Json::Int(i.pool_acquired)),
                        ("misses", Json::Int(i.pool_misses)),
                    ])),
                ]),
            }),
            ("service_camera", Json::Object(vec![
                ("start", camera_json(&self.service_start)),
                ("end", camera_json(&self.service_end)),
            ])),
        ])
    }

    /// The short table the tool prints.
    pub fn table(&self) -> String {
        let f = &self.frames;
        let mut rows: Vec<(String, String)> = Vec::new();
        let mut row = |k: &str, v: String| rows.push((k.to_string(), v));
        row("frames delivered", f.delivered.to_string());
        row("frames skipped by ring", self.skipped.to_string());
        row(
            "producer stalls (stalls, resumes, lost)",
            format!("{}, {}, {}", self.stalls, self.resumes, self.lost_resumes),
        );
        for (edge, ns) in f.stall_gaps_ns.iter().enumerate() {
            row(
                &format!("stall {}", edge + 1),
                format!("stall, {:.3} ms", *ns as f64 / 1e6),
            );
        }
        row(
            "gaps (count, largest)",
            format!("{}, {}", f.gaps, f.largest_gap),
        );
        row(
            "duplicates, out of order",
            format!("{}, {}", f.duplicates, f.out_of_order),
        );
        row("metadata refused", f.metadata_refused.to_string());
        row("measured rate (fps)", fmt_opt(f.measured_fps(), 3));
        row(
            "interval min/median/max (ns)",
            format!(
                "{} / {} / {}",
                fmt_int(f.intervals.min()),
                fmt_int(f.intervals.median()),
                fmt_int(f.intervals.max())
            ),
        );
        row(
            "requested rate (fps)",
            fmt_milli(self.requested_milli().map(u64::from)),
        );
        row(
            "achieved, backend (fps)",
            fmt_milli(self.achieved_milli().map(u64::from)),
        );
        row(
            "achieved, metadata (fps)",
            fmt_milli(self.achieved_metadata_milli.map(u64::from)),
        );
        row(
            "measured, service (fps)",
            fmt_milli(self.service_measured_milli().map(u64::from)),
        );
        row("measured, reader (fps)", fmt_opt(f.measured_fps(), 3));
        row(
            "rate reason",
            self.rate_reason().unwrap_or_else(|| {
                "none (requested and achieved agree, or nothing to compare)".to_string()
            }),
        );
        row(
            "sensor rate (fps)",
            format!("not reported ({})", Self::SENSOR_RATE_SOURCE),
        );
        row(
            "latency min/median/p99/max (ns)",
            format!(
                "{} / {} / {} / {}",
                fmt_int(f.latency.min()),
                fmt_int(f.latency.median()),
                fmt_int(f.latency.quantile(0.99)),
                fmt_int(f.latency.max())
            ),
        );
        let copies: usize = self.copies.iter().map(|c| c.copies).sum();
        row("copies (this process)", copies.to_string());
        let service_copies: usize = self.service_copies.iter().map(|c| c.copies).sum();
        row("copies (service)", service_copies.to_string());
        row(
            "reader CPU per frame (ns)",
            fmt_opt(Self::ns_per_frame(self.own.cpu_ns, f.delivered), 0),
        );
        if let Some(i) = &self.isp {
            row(
                "soft ISP time min/median/p99/max (ns)",
                format!(
                    "{} / {} / {} / {}",
                    fmt_int(i.times.min()),
                    fmt_int(i.times.median()),
                    fmt_int(i.times.quantile(0.99)),
                    fmt_int(i.times.max())
                ),
            );
            row(
                "soft ISP output pool",
                format!(
                    "{} buffers, {} acquired, {} misses, {} errors",
                    i.pool_buffers, i.pool_acquired, i.pool_misses, i.errors
                ),
            );
        }
        if let Some(c) = &self.service_end {
            row(
                "service state",
                format!("{} (health {})", c.state, c.health),
            );
            row(
                "service keep window",
                format!(
                    "keep {} of {} buffers, hardware reserve {}, reclaimed {}",
                    fmt_int(c.keep),
                    fmt_int(c.buffers),
                    fmt_int(c.hardware_reserve),
                    fmt_int(c.reclaimed)
                ),
            );
        }
        if let Some(mem) = self.own.memory_end {
            row(
                "reader RSS / HWM (bytes)",
                format!("{} / {}", mem.rss_bytes, mem.peak_bytes),
            );
        }
        let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let mut out = format!(
            "styx-measure: {} frames in {:.2} s ({})\n",
            f.delivered, self.wall_s, self.stop
        );
        for (k, v) in rows {
            out.push_str(&format!("  {k:<width$}  {v}\n"));
        }
        out
    }
}

impl Report {
    /// The rate asked for, in milli-fps: the operator's override, else the status's.
    pub fn requested_milli(&self) -> Option<u32> {
        self.asked_fps_milli.or_else(|| {
            self.service_end
                .as_ref()
                .or(self.service_start.as_ref())
                .and_then(|c| c.requested_milli)
                .and_then(|m| u32::try_from(m).ok())
        })
    }

    /// The rate the backend reports, from the status (the end of the run, else the start).
    pub fn achieved_milli(&self) -> Option<u32> {
        self.service_end
            .as_ref()
            .or(self.service_start.as_ref())
            .and_then(|c| c.achieved_milli)
            .and_then(|m| u32::try_from(m).ok())
    }

    /// The rate the service measured from its capture timestamps, at the end of the run.
    pub fn service_measured_milli(&self) -> Option<u32> {
        self.service_end
            .as_ref()?
            .measured_milli
            .and_then(|m| u32::try_from(m).ok())
    }

    /// The service's reason the achieved rate differs from the requested one, or `None`.
    pub fn rate_reason(&self) -> Option<String> {
        self.service_end
            .as_ref()
            .and_then(|c| c.rate_reason.clone())
            .map(|r| r.replace('_', " "))
    }
}

fn fmt_milli(v: Option<u64>) -> String {
    v.map_or("n/a".to_string(), |m| format!("{:.3}", m as f64 / 1000.0))
}

fn fmt_opt(v: Option<f64>, places: usize) -> String {
    match v {
        Some(v) => format!("{v:.places$}"),
        None => "n/a".to_string(),
    }
}

fn fmt_int(v: Option<u64>) -> String {
    v.map_or("n/a".to_string(), |v| v.to_string())
}
