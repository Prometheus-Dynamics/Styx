//! Side-by-side markdown table of several runs.

use crate::capture::RunResult;
use crate::timing::Spread;

fn spread(s: Option<Spread>) -> String {
    match s {
        Some(s) if s.count > 1 => format!(
            "{:.1} ({:.1}..{:.1}, n={})",
            s.median, s.min, s.max, s.count
        ),
        Some(s) => format!("{:.1}", s.median),
        None => "-".into(),
    }
}

fn kib(v: u64) -> String {
    if v >= 10 * 1024 {
        format!("{:.1} MiB", v as f64 / 1024.0)
    } else {
        format!("{v} KiB")
    }
}

fn means(v: &[f64]) -> String {
    v.iter()
        .map(|x| format!("{x:.3}"))
        .collect::<Vec<_>>()
        .join(" / ")
}

type Row = (&'static str, fn(&RunResult) -> String);

const ROWS: &[Row] = &[
    ("backend", |r| r.backend.clone()),
    ("mode / delivered", |r| {
        format!("{} / {}", r.mode, r.delivered_format)
    }),
    ("residency", |r| r.residency.clone()),
    ("probe (ms)", |r| format!("{:.1}", r.probe_ms)),
    ("start() returns (ms)", |r| spread(r.start.start_call_ms)),
    ("open → first frame (ms)", |r| {
        spread(r.start.first_frame_ms)
    }),
    ("open → AE converged (ms)", |r| {
        if r.start.convergence_source == "none" {
            "n/a (no AE)".into()
        } else {
            spread(r.start.converged_ms)
        }
    }),
    ("open → exposure settled (ms)", |r| {
        if r.start.exposure_source == "none" {
            "n/a".into()
        } else {
            spread(r.start.settled_ms)
        }
    }),
    ("requested / measured fps", |r| {
        format!(
            "{:.3} / {:.3}",
            r.timing.requested_fps, r.timing.measured_fps
        )
    }),
    ("fps error", |r| {
        format!("{:+.3}%", r.timing.fps_error_percent)
    }),
    ("interval mean ± sd (µs)", |r| {
        format!(
            "{:.1} ± {:.1}",
            r.timing.interval_mean_us, r.timing.interval_std_us
        )
    }),
    ("interval min..max (µs)", |r| {
        format!(
            "{:.1}..{:.1}",
            r.timing.interval_min_us, r.timing.interval_max_us
        )
    }),
    ("jitter p99 (µs)", |r| {
        format!("{:.1}", r.timing.jitter_p99_us)
    }),
    ("dropped (sequence / timestamps)", |r| {
        format!(
            "{} / {} of {}",
            r.timing
                .dropped_by_sequence
                .map_or("-".into(), |d| d.to_string()),
            r.timing.dropped_by_timestamp,
            r.timing.frames
        )
    }),
    ("CPU % of a core (process + children)", |r| {
        format!(
            "{:.1} ({:.1} + {:.1})",
            r.cpu.total_percent, r.cpu.process_percent, r.cpu.children_percent
        )
    }),
    ("RSS / PSS (tree)", |r| {
        format!("{} / {}", kib(r.memory.rss_kib), kib(r.memory.pss_kib))
    }),
    ("dma-bufs held (count, size)", |r| {
        format!("{}, {}", r.memory.dmabuf_count, kib(r.memory.dmabuf_kib))
    }),
    ("system dma-buf delta", |r| {
        r.memory
            .system_dmabuf_delta_kib
            .map_or("-".into(), |d| kib(d.max(0) as u64))
    }),
    ("image kind", |r| {
        r.image.as_ref().map_or("-".into(), |i| i.kind.clone())
    }),
    ("mean level (0..1)", |r| {
        r.image
            .as_ref()
            .map_or("-".into(), |i| format!("{:.3}", i.mean))
    }),
    ("R / G / B means", |r| {
        r.image.as_ref().map_or("-".into(), |i| means(&i.rgb_means))
    }),
    ("clipped / dark fraction", |r| {
        r.image.as_ref().map_or("-".into(), |i| {
            format!("{:.4} / {:.4}", i.clipped_fraction, i.dark_fraction)
        })
    }),
];

/// A markdown table, one column per run, then each run's notes.
pub fn markdown(runs: &[RunResult]) -> String {
    let mut out = String::from("| metric |");
    for r in runs {
        out.push_str(&format!(" {} |", r.label));
    }
    out.push_str("\n|---|");
    out.push_str(&"---|".repeat(runs.len()));
    out.push('\n');
    for (name, cell) in ROWS {
        out.push_str(&format!("| {name} |"));
        for r in runs {
            out.push_str(&format!(" {} |", cell(r).replace('|', "\\|")));
        }
        out.push('\n');
    }
    for r in runs.iter().filter(|r| !r.notes.is_empty()) {
        out.push_str(&format!("\n{}:\n", r.label));
        for n in &r.notes {
            out.push_str(&format!("- {n}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_a_column_per_run() {
        let a = RunResult {
            label: "libcamera".into(),
            notes: vec!["x".into()],
            ..RunResult::default()
        };
        let b = RunResult {
            label: "native".into(),
            ..RunResult::default()
        };
        let md = markdown(&[a, b]);
        assert!(md.starts_with("| metric | libcamera | native |"));
        assert!(
            md.lines()
                .all(|l| !l.starts_with('|') || l.matches('|').count() == 4)
        );
        assert!(md.contains("\nlibcamera:\n- x"));
    }
}
