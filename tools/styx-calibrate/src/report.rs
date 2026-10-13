//! Markdown reports: a dark sweep's, and a processed grey capture's.

use std::fmt::Write;

use crate::dark::{DefectPixel, DefectReason, Sweep};
use crate::overlay::Provenance;
use crate::processed::ProcessedStats;

/// The most defect pixels listed in the report (the overlay has all of them).
pub const MAX_DEFECTS_LISTED: usize = 200;

/// The dark sweep's report.
pub fn dark(sweep: &Sweep, prov: &Provenance, defects: &[DefectPixel]) -> String {
    let mut s = String::new();
    let first = &sweep.captures[0];
    let _ = writeln!(s, "# Sensor calibration: {}", prov.sensor);
    let _ = writeln!(s);
    let unit = prov.unit.as_deref().unwrap_or("not given");
    let _ = writeln!(
        s,
        "- Sensor `{}`, unit {unit}, date {}, Styx commit `{}`.",
        prov.sensor, prov.date, prov.styx_commit
    );
    let temps: Vec<(f64, f64)> = sweep
        .captures
        .iter()
        .filter_map(|c| c.temperature)
        .collect();
    match (
        temps.iter().map(|t| t.0).reduce(f64::min),
        temps.iter().map(|t| t.1).reduce(f64::max),
    ) {
        (Some(lo), Some(hi)) => {
            let _ = writeln!(s, "- Temperature {lo:.1} to {hi:.1} °C over the frames.");
        }
        _ => {
            let _ = writeln!(s, "- Temperature not recorded.");
        }
    }
    let _ = writeln!(
        s,
        "- {} dark captures at {} gains ({}-bit raw, {} frames in the first).",
        sweep.captures.len(),
        sweep.gains.len(),
        first.bits,
        first.frames
    );
    let _ = writeln!(s);

    let _ = writeln!(s, "## Captures");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "All levels and noise are raw DN ({}-bit). Defect pixels are left out of the means and of the \
         spreads.",
        first.bits
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "| exposure µs | gain | steady | frames | black R | black Gr | black Gb | black B | read noise | DSNU | row FPN | col FPN | row noise | col noise | 0 % | defects |"
    );
    let _ = writeln!(
        s,
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for c in &sweep.captures {
        let ch = &c.channels;
        let avg = |f: fn(&crate::dark::ChannelStats) -> f64| ch.iter().map(f).sum::<f64>() / 4.0;
        let _ = writeln!(
            s,
            "| {:.0} | {:.3} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.2} | {} |",
            c.exposure_us,
            c.analog_gain,
            if c.steady { "yes" } else { "no" },
            c.frames,
            ch[0].black_median,
            ch[1].black_median,
            ch[2].black_median,
            ch[3].black_median,
            avg(|c| c.read_noise),
            avg(|c| c.dsnu),
            avg(|c| c.row_fpn),
            avg(|c| c.col_fpn),
            avg(|c| c.row_noise),
            avg(|c| c.col_noise),
            100.0 * avg(|c| c.clipped_zero),
            c.defects.len(),
        );
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "Black levels above are the robust medians of the per-pixel temporal means; \
         the means of all samples are in the table below."
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "| exposure µs | gain | black mean R | Gr | Gb | B |");
    let _ = writeln!(s, "|---|---|---|---|---|---|");
    for c in &sweep.captures {
        let m = |i: usize| c.channels[i].black_mean;
        let _ = writeln!(
            s,
            "| {:.0} | {:.3} | {:.2} | {:.2} | {:.2} | {:.2} |",
            c.exposure_us,
            c.analog_gain,
            m(0),
            m(1),
            m(2),
            m(3)
        );
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "## Black level by gain");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{}",
        if sweep.black_moves {
            "The level moves with gain by more than the threshold: the overlay writes a list per gain."
        } else {
            "The level holds across gains within the threshold: the overlay writes one level (the mean over gains)."
        }
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "| gain | captures | R | Gr | Gb | B |");
    let _ = writeln!(s, "|---|---|---|---|---|---|");
    for g in &sweep.gains {
        let _ = writeln!(
            s,
            "| {:.3} | {} | {:.2} | {:.2} | {:.2} | {:.2} |",
            g.gain, g.captures, g.black[0], g.black[1], g.black[2], g.black[3]
        );
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "## Read noise by gain");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "| gain | read noise (DN, mean of the channels) | per channel R / Gr / Gb / B | noise constant (16-bit, read noise × 2^(16−bits) / √gain) |"
    );
    let _ = writeln!(s, "|---|---|---|---|");
    let scale = first.scale16();
    for g in &sweep.gains {
        let r = g.read_noise;
        let _ = writeln!(
            s,
            "| {:.3} | {:.3} | {:.3} / {:.3} / {:.3} / {:.3} | {:.2} |",
            g.gain,
            g.read_noise_all(),
            r[0],
            r[1],
            r[2],
            r[3],
            g.read_noise_all() * scale / g.gain.max(1e-9).sqrt()
        );
    }
    let _ = writeln!(s);
    match sweep.read_noise_fit {
        Some((c, k)) => {
            let _ = writeln!(
                s,
                "Fit over the gains: read noise ≈ {c:.3} + {k:.3} × gain (DN, raw). The noise constant \
                 the denoise code uses scales with √gain, so it is fitted per gain, not as a line."
            );
        }
        None => {
            let _ = writeln!(s, "A line over the gains needs two gains.");
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "## Dark current");
    let _ = writeln!(s);
    if sweep.dark_current.is_empty() {
        let _ = writeln!(s, "Needs two or more exposures at one gain.");
    } else {
        let _ = writeln!(
            s,
            "| gain | exposures | slope (DN/s, raw) | intercept (DN, raw) |"
        );
        let _ = writeln!(s, "|---|---|---|---|");
        for d in &sweep.dark_current {
            let _ = writeln!(
                s,
                "| {:.3} | {} | {:.3} | {:.2} |",
                d.gain, d.exposures, d.slope_per_s, d.intercept
            );
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "## Fixed pattern and defects");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{} defect pixels: {} hot, {} stuck, {} noisy.",
        defects.len(),
        count(defects, DefectReason::Hot),
        count(defects, DefectReason::Stuck),
        count(defects, DefectReason::Noisy)
    );
    if !defects.is_empty() {
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "| x | y | channel | reason | temporal mean | temporal σ |"
        );
        let _ = writeln!(s, "|---|---|---|---|---|---|");
        for d in defects.iter().take(MAX_DEFECTS_LISTED) {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {:.2} | {:.3} |",
                d.x,
                d.y,
                d.channel.name(),
                d.reason.name(),
                d.mean,
                d.sigma
            );
        }
        if defects.len() > MAX_DEFECTS_LISTED {
            let _ = writeln!(
                s,
                "\n… and {} more (in the overlay).",
                defects.len() - MAX_DEFECTS_LISTED
            );
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "## Notes");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "- Black levels and noise constants in the overlay are on the 16-bit scale the back end \
         uses (raw × {scale}); the table above is raw DN."
    );
    let _ = writeln!(
        s,
        "- The denoise noise model is `noise ≈ constant + slope × √level` with both scaled by √(analogue gain). \
         At a level of zero (black), the dark read noise is therefore the constant alone: the overlay's \
         `rpi.noise.reference_constant` is the read noise at unity gain's scale (read noise / √gain), and \
         `by_gain` gives it per gain. The slope is not measured by darks (it needs flats) and keeps the \
         tuning's value."
    );
    let _ = writeln!(
        s,
        "- Read noise is the temporal standard deviation per pixel, averaged over the channel. DSNU is the \
         spatial standard deviation of the per-pixel temporal means. Row and column FPN are the spread of \
         each row's and column's temporal mean."
    );
    s
}

fn count(defects: &[DefectPixel], r: DefectReason) -> usize {
    defects.iter().filter(|d| d.reason == r).count()
}

/// The processed grey capture's report (the before and after metric).
pub fn processed(stats: &[ProcessedStats]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Processed dark check");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "Clipped black is the fraction of pixels that are exactly 0. A black level that is right at every \
         gain keeps it near zero, not 40–55 % (the symmetric noise around black clips half of it when the \
         output has no pedestal)."
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "| capture | exposure µs | gain | frames | size | pixels = 0 | mean | temporal noise (mean) | temporal noise (p99) |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|");
    for p in stats {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {}x{} | {:.2} % | {:.3} | {:.3} | {:.3} |",
            p.dir.display(),
            p.exposure_us.map_or("?".into(), |v| format!("{v:.0}")),
            p.analog_gain.map_or("?".into(), |v| format!("{v:.3}")),
            p.frames,
            p.size.0,
            p.size.1,
            100.0 * p.clipped,
            p.mean,
            p.temporal_mean,
            p.temporal_p99
        );
    }
    s
}
