//! Per-frame log and the run summary.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use styx_pipeline::measure::{overshoot, settle_index};
use styx_pipeline::{SensorValues, Step};

use crate::{Args, PERTURB_FRAMES};

/// One frame.
#[derive(Clone, Debug, Default)]
pub struct FrameLog {
    pub seq: u64,
    pub timestamp: Duration,
    pub exposure_us: f64,
    pub gain: f64,
    pub verified: bool,
    pub measured_y: f64,
    pub target_y: f64,
    pub locked: bool,
    pub wb: [f64; 3],
    pub ct: f64,
    pub isp_dg: f64,
    /// Mean output luma, 0..255.
    pub out_y: f64,
    /// Sensor timestamp to output ready.
    pub latency: Duration,
    /// Processing time on the host for this frame.
    pub processing: Duration,
    pub request_lands: Option<u64>,
    /// AE's flicker estimate for this frame (relative brightness from the flicker).
    pub flicker_mod: f64,
    /// The flicker period AE detected, microseconds (0: none).
    pub flicker_us: u64,
    /// Where the processing time went (software ISP loop only).
    pub timing: Option<styx_pipeline::SoftTiming>,
}

impl FrameLog {
    pub fn new(sensor: &SensorValues, step: &Step, timestamp: Duration) -> Self {
        let p = &step.params;
        Self {
            seq: sensor.frame,
            timestamp,
            exposure_us: sensor.exposure.as_secs_f64() * 1e6,
            gain: sensor.analogue_gain * sensor.digital_gain,
            verified: sensor.verified,
            measured_y: p.ae.measured_y,
            target_y: p.ae.target_y,
            locked: p.ae.locked,
            wb: step.isp.wb,
            ct: p.colour_temperature,
            isp_dg: step.isp.digital_gain,
            flicker_mod: p.ae.flicker_modulation,
            flicker_us: p.ae.flicker_detected.map_or(0, |d| d.as_micros() as u64),
            ..Default::default()
        }
    }

    pub fn total(&self) -> f64 {
        self.exposure_us * self.gain
    }

    pub fn line(&self) -> String {
        format!(
            "seq {:4} exp {:8.1} us gain {:5.2}{} | Y {:.3}/{:.3}{} | wb {:.2}/{:.2} {:4.0} K dg {:.2} | out Y {:5.1} | lat {:5.1} ms proc {:5.2} ms{}",
            self.seq,
            self.exposure_us,
            self.gain,
            if self.verified { "v" } else { " " },
            self.measured_y,
            self.target_y,
            if self.locked { " L" } else { "  " },
            self.wb[0],
            self.wb[2],
            self.ct,
            self.isp_dg,
            self.out_y,
            self.latency.as_secs_f64() * 1e3,
            self.processing.as_secs_f64() * 1e3,
            self.request_lands
                .map_or(String::new(), |f| format!(" -> req lands {f}"))
        )
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn percentile(mut v: Vec<f64>, q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// Convergence of a segment `[from, to)`: frames until the sensor's total exposure and the
/// output level stay within 5% of their value at the segment's end.
fn convergence(frames: &[FrameLog], from: usize, to: usize, out: &mut String) {
    if to <= from + 2 {
        return;
    }
    let seg = &frames[from..to];
    let total: Vec<f64> = seg.iter().map(FrameLog::total).collect();
    let y: Vec<f64> = seg.iter().map(|f| f.out_y).collect();
    let locked = seg.iter().position(|f| f.locked);
    let s_total = settle_index(&total, 0.05).unwrap_or(0);
    let s_y = settle_index(&y, 0.05).unwrap_or(0);
    let _ = writeln!(
        out,
        "  frames {}..{}: exposure x gain within 5% after {} frames (overshoot {:.1}%), output level within 5% after {} frames, AE locked after {}; final {:.0} us x {:.2}, out Y {:.1}",
        seg[0].seq,
        seg[seg.len() - 1].seq,
        s_total,
        100.0 * overshoot(&total, s_total),
        s_y,
        locked.map_or("never".into(), |l| l.to_string()),
        seg[seg.len() - 1].exposure_us,
        seg[seg.len() - 1].gain,
        seg[seg.len() - 1].out_y,
    );
}

/// Relative standard deviation.
fn rel_sd(v: &[f64]) -> f64 {
    let m = v.iter().sum::<f64>() / v.len().max(1) as f64;
    (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt() / m.max(1e-12)
}

/// Frame-to-frame stability over the second half of the run (or after the last forced step):
/// how much AE moved and how much the frames' brightness varied.
fn stability(frames: &[FrameLog], from: usize, out: &mut String) {
    let seg = &frames[from.min(frames.len())..];
    if seg.len() < 4 {
        return;
    }
    let total: Vec<f64> = seg.iter().map(FrameLog::total).collect();
    let changes = total
        .windows(2)
        .filter(|w| (w[1] / w[0] - 1.0).abs() > 0.005)
        .count();
    let y: Vec<f64> = seg.iter().map(|f| f.measured_y).collect();
    let out_y: Vec<f64> = seg.iter().map(|f| f.out_y).collect();
    let locked = seg.iter().filter(|f| f.locked).count();
    let detected = frames.iter().position(|f| f.flicker_us > 0);
    let _ = writeln!(
        out,
        "steady state, frames {}..{}: exposure x gain SD {:.2}% ({} changes > 0.5%), metered Y SD {:.2}%, output Y SD {:.2}%, AE locked on {:.1}% of frames; flicker detected {}",
        seg[0].seq,
        seg[seg.len() - 1].seq,
        100.0 * rel_sd(&total),
        changes,
        100.0 * rel_sd(&y),
        100.0 * rel_sd(&out_y),
        100.0 * locked as f64 / seg.len() as f64,
        detected.map_or("never".into(), |i| format!(
            "{} us at seq {}",
            frames[i].flicker_us, frames[i].seq
        )),
    );
}

/// The run's summary.
pub struct Summary<'a> {
    pub name: &'a str,
    pub frames: &'a [FrameLog],
    pub open_to_first: Duration,
    pub cpu: Duration,
    pub wall: Duration,
    pub peak_rss: u64,
    pub extra: Vec<String>,
}

impl Summary<'_> {
    pub fn render(&self, a: &Args) -> String {
        let f = self.frames;
        let mut out = String::new();
        let _ = writeln!(out, "== {} summary ({} frames)", self.name, f.len());
        if f.len() < 2 {
            return out;
        }
        let span = f[f.len() - 1].timestamp.saturating_sub(f[0].timestamp);
        let seq_span = f[f.len() - 1].seq - f[0].seq;
        let _ = writeln!(
            out,
            "frame rate {:.3} fps over {} frames ({} sequence gaps), open -> first frame {:.1} ms",
            seq_span as f64 / span.as_secs_f64().max(1e-9),
            f.len(),
            seq_span + 1 - f.len() as u64,
            ms(self.open_to_first)
        );
        let lat: Vec<f64> = f.iter().map(|x| ms(x.latency)).collect();
        let proc: Vec<f64> = f.iter().map(|x| ms(x.processing)).collect();
        let _ = writeln!(
            out,
            "latency sensor timestamp -> output: median {:.2} ms, p95 {:.2} ms; processing per frame median {:.2} ms, p95 {:.2} ms",
            percentile(lat.clone(), 0.5),
            percentile(lat, 0.95),
            percentile(proc.clone(), 0.5),
            percentile(proc, 0.95)
        );
        let _ = writeln!(
            out,
            "CPU {:.1} ms per frame ({:.1}% of one core over {:.2} s), peak RSS {:.1} MiB",
            ms(self.cpu) / f.len() as f64,
            100.0 * self.cpu.as_secs_f64() / self.wall.as_secs_f64().max(1e-9),
            self.wall.as_secs_f64(),
            self.peak_rss as f64 / (1024.0 * 1024.0)
        );
        let timed: Vec<_> = f.iter().filter_map(|x| x.timing).collect();
        if !timed.is_empty() {
            let med = |g: &dyn Fn(&styx_pipeline::SoftTiming) -> Duration| {
                percentile(timed.iter().map(|t| ms(g(t))).collect(), 0.5)
            };
            let mean = |g: &dyn Fn(&styx_pipeline::SoftTiming) -> Duration| {
                timed.iter().map(|t| ms(g(t))).sum::<f64>() / timed.len() as f64
            };
            let _ = writeln!(
                out,
                "loop per frame, median / mean ms: settings {:.3} / {:.3}, ISP {:.3} / {:.3}, statistics conversion {:.3} / {:.3}, algorithms {:.3} / {:.3}",
                med(&|t| t.settings),
                mean(&|t| t.settings),
                med(&|t| t.isp),
                mean(&|t| t.isp),
                med(&|t| t.stats),
                mean(&|t| t.stats),
                med(&|t| t.algorithms),
                mean(&|t| t.algorithms),
            );
        }
        let verified = f.iter().filter(|x| x.verified).count();
        let _ = writeln!(
            out,
            "sensor values read back from embedded data on {verified} of {} frames",
            f.len()
        );
        let _ = writeln!(out, "AE convergence:");
        let mut bounds: Vec<(usize, &str)> = vec![(0, "start")];
        for (at, k) in &a.perturb {
            let release = at + PERTURB_FRAMES;
            if let Some(i) = f.iter().position(|x| x.seq >= release) {
                bounds.push((
                    i,
                    if *k < 1.0 {
                        "after a darker step"
                    } else {
                        "after a brighter step"
                    },
                ));
            }
        }
        for (n, (from, what)) in bounds.iter().enumerate() {
            let to = match a.perturb.get(n) {
                Some((at, _)) => f.iter().position(|x| x.seq >= *at).unwrap_or(f.len()),
                None => f.len(),
            };
            let _ = writeln!(out, " {what}:");
            convergence(f, *from, to, &mut out);
        }
        let steady_from = a
            .perturb
            .iter()
            .filter_map(|(at, _)| f.iter().position(|x| x.seq >= at + 2 * PERTURB_FRAMES))
            .max()
            .unwrap_or(0)
            .max(f.len() / 2);
        stability(f, steady_from, &mut out);
        if let Some(first) = f.iter().position(|x| x.locked) {
            let t = self.open_to_first + f[first].timestamp.saturating_sub(f[0].timestamp);
            let _ = writeln!(
                out,
                "startup: open -> first AE-locked frame (seq {}) {:.1} ms",
                f[first].seq,
                ms(t)
            );
        }
        let last = &f[f.len() - 1];
        let _ = writeln!(
            out,
            "AWB: gains R {:.3} B {:.3}, {:.0} K",
            last.wb[0], last.wb[2], last.ct
        );
        for e in &self.extra {
            let _ = writeln!(out, "{e}");
        }
        out
    }
}

/// Writes the per-frame log as CSV.
pub fn write_csv(path: &Path, frames: &[FrameLog]) -> std::io::Result<()> {
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(
        w,
        "seq,timestamp_ns,exposure_us,gain,verified,measured_y,target_y,locked,wb_r,wb_b,ct,isp_dg,out_y,latency_ms,processing_ms,request_lands,flicker_mod,flicker_us"
    )?;
    for f in frames {
        writeln!(
            w,
            "{},{},{:.1},{:.4},{},{:.5},{:.5},{},{:.4},{:.4},{:.0},{:.4},{:.2},{:.3},{:.3},{},{:.4},{}",
            f.seq,
            f.timestamp.as_nanos(),
            f.exposure_us,
            f.gain,
            u8::from(f.verified),
            f.measured_y,
            f.target_y,
            u8::from(f.locked),
            f.wb[0],
            f.wb[2],
            f.ct,
            f.isp_dg,
            f.out_y,
            ms(f.latency),
            ms(f.processing),
            f.request_lands.map_or(String::new(), |v| v.to_string()),
            f.flicker_mod,
            f.flicker_us
        )?;
    }
    w.flush()
}
