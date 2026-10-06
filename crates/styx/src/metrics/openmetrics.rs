//! Snapshots as Prometheus text exposition (version 0.0.4, which OpenMetrics scrapers also
//! read), and with the `metrics-http` feature a tiny HTTP endpoint serving it.

use std::fmt::Write;

use super::camera::{CameraMetrics, ConsumerMetrics, Window};
use super::registry::{MetricsSnapshot, ServiceMetrics};

/// Escapes a label value.
pub(super) fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[derive(Default)]
pub(super) struct Text {
    pub(super) out: String,
    declared: std::collections::HashSet<&'static str>,
}

impl Text {
    pub(super) fn sample(
        &mut self,
        name: &'static str,
        kind: &str,
        help: &str,
        labels: &str,
        value: f64,
    ) {
        if self.declared.insert(name) {
            let _ = writeln!(self.out, "# HELP {name} {help}");
            let _ = writeln!(self.out, "# TYPE {name} {kind}");
        }
        if labels.is_empty() {
            let _ = writeln!(self.out, "{name} {value}");
        } else {
            let _ = writeln!(self.out, "{name}{{{labels}}} {value}");
        }
    }

    pub(super) fn gauge(
        &mut self,
        name: &'static str,
        help: &str,
        labels: &str,
        value: Option<f64>,
    ) {
        if let Some(v) = value {
            self.sample(name, "gauge", help, labels, v);
        }
    }

    pub(super) fn counter(&mut self, name: &'static str, help: &str, labels: &str, value: u64) {
        self.sample(name, "counter", help, labels, value as f64);
    }

    pub(super) fn window(&mut self, name: &'static str, help: &str, labels: &str, w: &Window) {
        for (q, v) in [
            ("0.5", w.p50_ms),
            ("0.95", w.p95_ms),
            ("0.99", w.p99_ms),
            ("1", w.max_ms),
        ] {
            self.gauge(name, help, &format!("{labels},quantile=\"{q}\""), v);
        }
    }

    fn camera(&mut self, c: &CameraMetrics) {
        let l = format!(
            "camera=\"{}\",id=\"{}\",backend=\"{}\"",
            esc(&c.name),
            c.id,
            esc(&c.backend)
        );
        for (stage, v) in [
            ("captured", c.frames.captured),
            ("delivered", c.frames.delivered),
            ("received", c.frames.received),
        ] {
            self.counter(
                "styx_camera_frames_total",
                "Frames by stage: produced by the backend, queued for the consumer, received.",
                &format!("{l},stage=\"{stage}\""),
                v,
            );
        }
        for (kind, v) in [
            ("configured", c.fps.configured),
            ("measured", c.fps.measured),
            ("average", c.fps.average),
        ] {
            self.gauge(
                "styx_camera_fps",
                "Frame rate: configured, measured from sensor timestamps, average since start.",
                &format!("{l},kind=\"{kind}\""),
                v,
            );
        }
        let d = &c.drops;
        for (cause, v) in [
            ("sensor_sequence_gap", d.sensor_sequence_gaps),
            ("queue_overflow", d.queue_overflow),
            ("corrupted", d.corrupted),
            ("isp_skipped", d.isp_skipped),
        ] {
            self.counter(
                "styx_camera_drops_total",
                "Frames lost, by cause.",
                &format!("{l},cause=\"{cause}\""),
                v,
            );
        }
        for (path, w) in [
            ("sensor_to_delivery", &c.latency.sensor_to_delivery),
            ("sensor_to_receive", &c.latency.sensor_to_receive),
        ] {
            self.window(
                "styx_camera_latency_ms",
                "Sensor timestamp to delivery or receipt, over the last frames (quantile 1: max).",
                &format!("{l},path=\"{path}\""),
                w,
            );
        }
        if let Some(isp) = &c.isp {
            for (stage, w) in [("isp", &isp.isp), ("processing", &isp.processing)] {
                self.window(
                    "styx_camera_isp_ms",
                    "ISP time per frame over the last frames (quantile 1: max).",
                    &format!("{l},isp=\"{}\",stage=\"{stage}\"", esc(&isp.kind)),
                    w,
                );
            }
        }
        self.sample(
            "styx_camera_cpu_seconds_total",
            "counter",
            "CPU time of the capture's worker threads.",
            &l,
            c.cpu.total_ns as f64 / 1e9,
        );
        self.gauge(
            "styx_camera_cpu_per_frame_us",
            "Worker thread CPU time per captured frame, in microseconds.",
            &l,
            c.cpu.per_frame_us,
        );
        if let Some(a) = &c.aaa {
            let state = a
                .ae_state
                .as_deref()
                .map(|s| f64::from(u8::from(s == "converged")));
            for (name, help, v) in [
                (
                    "styx_camera_ae_converged",
                    "AE has converged (1) or is searching (0).",
                    state,
                ),
                (
                    "styx_camera_exposure_us",
                    "Exposure of the latest frame.",
                    a.exposure_us,
                ),
                (
                    "styx_camera_analogue_gain",
                    "Analogue gain of the latest frame.",
                    a.analogue_gain,
                ),
                (
                    "styx_camera_digital_gain",
                    "Sensor digital gain of the latest frame.",
                    a.digital_gain,
                ),
                (
                    "styx_camera_colour_temperature_kelvin",
                    "AWB colour temperature.",
                    a.colour_temperature_k,
                ),
                ("styx_camera_lux", "Scene illuminance estimate.", a.lux),
                (
                    "styx_camera_flicker_hz",
                    "Light flicker AE detected.",
                    a.flicker_hz,
                ),
            ] {
                self.gauge(name, help, &l, v);
            }
            if let Some(state) = &a.af_state {
                self.gauge(
                    "styx_camera_af_state",
                    "AF state (1 for the state and mode reported).",
                    &format!(
                        "{l},state=\"{state}\",mode=\"{}\"",
                        a.af_mode.as_deref().unwrap_or("")
                    ),
                    Some(1.0),
                );
            }
            self.gauge(
                "styx_camera_lens_position_dioptres",
                "Lens position AF commanded.",
                &l,
                a.lens_position_dioptres,
            );
            self.gauge(
                "styx_camera_lens_settled",
                "The lens had settled for the latest frame (1) or was moving (0).",
                &l,
                a.lens_settled.map(|s| f64::from(u8::from(s))),
            );
            if let Some(scans) = a.af_scans {
                self.counter("styx_camera_af_scans_total", "AF scans started.", &l, scans);
            }
        }
        let s = &c.stills;
        if s.requests > 0 {
            for (kind, v) in [
                ("requests", s.requests),
                ("failed", s.failed),
                ("shots", s.shots),
                ("landed", s.landed),
                ("missed", s.missed),
            ] {
                self.counter(
                    "styx_camera_stills_total",
                    "Still requests and shots; shots that landed on their frame or not.",
                    &format!("{l},kind=\"{kind}\""),
                    v,
                );
            }
            self.window(
                "styx_camera_still_latency_ms",
                "Still request to ready, over the last stills (quantile 1: max).",
                &l,
                &s.latency,
            );
        }
        let r = &c.restarts;
        self.counter(
            "styx_camera_reconnects_total",
            "Times frames resumed after a reconnect.",
            &l,
            r.reconnects,
        );
        self.counter(
            "styx_camera_reconnect_attempts_total",
            "Attempts to reconnect a lost or stalled camera.",
            &l,
            r.reconnect_attempts,
        );
        let b = &c.buffers;
        for (name, help, v) in [
            (
                "styx_camera_queue_depth",
                "Frames waiting in the consumer queue.",
                b.queue_depth,
            ),
            (
                "styx_camera_queue_capacity",
                "Capacity of the consumer queue.",
                b.queue_capacity,
            ),
            (
                "styx_camera_buffers_held",
                "Capture buffers held by frames.",
                b.held,
            ),
            (
                "styx_camera_buffers_held_bytes",
                "Bytes of capture buffers held by frames.",
                b.held_bytes,
            ),
        ] {
            self.gauge(name, help, &l, Some(v as f64));
        }
        self.window(
            "styx_camera_buffer_hold_ms",
            "Capture buffer delivery to release (quantile 1: max).",
            &l,
            &b.hold,
        );
        self.hops(super::path::CAMERA_HOPS, &l, &c.path);
        for consumer in &c.consumers {
            self.consumer(&l, consumer);
        }
    }

    fn consumer(&mut self, labels: &str, c: &ConsumerMetrics) {
        let l = format!("{labels},consumer=\"{}\"", esc(&c.label));
        self.counter(
            "styx_consumer_frames_total",
            "Frames a consumer received.",
            &l,
            c.received,
        );
        self.counter(
            "styx_consumer_dropped_total",
            "Frames a consumer did not get.",
            &l,
            c.dropped,
        );
        self.gauge(
            "styx_consumer_held",
            "Frames a consumer holds.",
            &l,
            Some(c.held as f64),
        );
        self.window(
            "styx_consumer_hold_ms",
            "Time a consumer held each frame (quantile 1: max).",
            &l,
            &c.hold,
        );
        if let Some(h) = &c.hops {
            self.hops(super::path::CONSUMER_HOPS, &l, h);
        }
    }

    pub(super) fn snapshot(&mut self, s: &MetricsSnapshot) {
        let p = &s.process;
        for (name, help, v) in [
            (
                "styx_process_cameras_open",
                "Captures running in the process.",
                p.cameras_open as f64,
            ),
            (
                "styx_process_service_clients",
                "Camera service clients connected.",
                p.service_clients as f64,
            ),
            (
                "styx_process_rss_bytes",
                "Resident memory.",
                p.rss_bytes as f64,
            ),
            ("styx_process_threads", "Threads.", p.threads as f64),
            (
                "styx_process_dmabufs",
                "Distinct dma-bufs open.",
                p.dmabufs as f64,
            ),
            (
                "styx_process_dmabuf_bytes",
                "Bytes of the dma-bufs open.",
                p.dmabuf_bytes as f64,
            ),
        ] {
            self.gauge(name, help, "", Some(v));
        }
        self.sample(
            "styx_process_cpu_seconds_total",
            "counter",
            "User and system CPU time of the process.",
            "",
            p.cpu_ns as f64 / 1e9,
        );
        self.path(&p.path);
        for c in &s.cameras {
            self.camera(c);
        }
    }
}

impl MetricsSnapshot {
    /// As Prometheus text exposition (`text/plain; version=0.0.4`).
    pub fn prometheus_text(&self) -> String {
        let mut t = Text::default();
        t.snapshot(self);
        t.out
    }
}

impl ServiceMetrics {
    /// As Prometheus text exposition: the service's counters, its clients, and the snapshot of
    /// its process.
    pub fn prometheus_text(&self) -> String {
        let mut t = Text::default();
        t.gauge(
            "styx_service_clients",
            "Clients connected.",
            "",
            Some(self.clients as f64),
        );
        for (what, v) in [
            ("rejected", self.rejected),
            ("unauthorized", self.unauthorized),
            ("restarts", self.restarts),
            ("sent", self.sent),
            ("copied", self.copied),
            ("skipped", self.skipped),
            ("revoked", self.revoked),
        ] {
            t.counter(
                "styx_service_events_total",
                "Camera service events.",
                &format!("event=\"{what}\""),
                v,
            );
        }
        for c in &self.client_metrics {
            t.consumer("service=\"camera\"", c);
        }
        t.snapshot(&self.snapshot);
        t.out
    }
}

/// Serves [`snapshot`](super::snapshot) as Prometheus text over HTTP (any path) until dropped.
#[cfg(feature = "metrics-http")]
pub struct MetricsHttpServer {
    addr: std::net::SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "metrics-http")]
impl MetricsHttpServer {
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

#[cfg(feature = "metrics-http")]
impl Drop for MetricsHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Listen on `addr` (e.g. `127.0.0.1:9464`) and answer every HTTP request with the metrics of
/// this process as Prometheus text. One small thread; requests are served one at a time.
#[cfg(feature = "metrics-http")]
pub fn serve_http(addr: impl std::net::ToSocketAddrs) -> std::io::Result<MetricsHttpServer> {
    use std::io::{Read, Write as _};
    use std::sync::atomic::Ordering;
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("styx-metrics-http".into())
        .spawn(move || {
            while !flag.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        continue;
                    }
                    Err(_) => continue,
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                // The request itself does not matter: read what arrived, answer, close.
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let body = super::snapshot().prometheus_text();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        })?;
    crate::trace::info!(%addr, "metrics served over HTTP");
    Ok(MetricsHttpServer {
        addr,
        stop,
        thread: Some(thread),
    })
}
