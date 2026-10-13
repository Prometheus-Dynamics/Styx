//! The tuning overlay: a sweep's black level, noise constants and defect pixels as the JSON
//! file Styx merges over the sensor's tuning (see `docs/calibration.md`).
//!
//! The blocks are the Raspberry Pi tuning's (`rpi.black_level`, `rpi.noise`), so libcamera's
//! keys keep their meaning: levels and noise constants are on the 16-bit scale (raw samples
//! times `2^(16 - bits)`). Styx's extensions are the per-channel `black_level_gr` and
//! `black_level_gb`, the `by_gain` lists, and the top-level `defect_pixels`, `measured` and
//! provenance keys (which Styx ignores beyond what it reads).

use serde_json::{Map, Value, json};

use crate::dark::{DefectPixel, Sweep};
use crate::raw::{Error, Result};

/// The overlay format version written.
pub const OVERLAY_VERSION: u32 = 1;

/// What the overlay says about where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Provenance {
    /// The sensor (the tuning's name stem, `ov9782`).
    pub sensor: String,
    /// The unit, when given (a serial or a name).
    pub unit: Option<String>,
    /// The date, `YYYY-MM-DD`.
    pub date: String,
    /// The Styx commit the tool ran from (`unknown` when not in a checkout).
    pub styx_commit: String,
}

/// Rounds to two decimals, so the file is readable and stable.
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The tuning overlay for `sweep`, as JSON. Fails if the overlay would not parse in Styx.
pub fn build(sweep: &Sweep, prov: &Provenance, defects: &[DefectPixel]) -> Result<Value> {
    let first = sweep
        .captures
        .first()
        .ok_or_else(|| Error("no captures".into()))?;
    let scale = first.scale16();
    let scaled = |v: f64| r2(v * scale);
    let level_block = |b: [f64; 4]| -> Map<String, Value> {
        let [r, gr, gb, bl] = b.map(scaled);
        let mut m = Map::new();
        m.insert("black_level_r".into(), json!(r));
        m.insert("black_level_gr".into(), json!(gr));
        m.insert("black_level_gb".into(), json!(gb));
        m.insert("black_level_g".into(), json!(r2((gr + gb) / 2.0)));
        m.insert("black_level_b".into(), json!(bl));
        m.insert("black_level".into(), json!(r2((r + gr + gb + bl) / 4.0)));
        m
    };

    // Black levels: one set where the level holds across gains, else the reference gain's
    // (the lowest) with a list per gain.
    let black = if sweep.black_moves && sweep.gains.len() >= 2 {
        let mut m = level_block(sweep.gains[0].black);
        let list: Vec<Value> = sweep
            .gains
            .iter()
            .map(|g| {
                let mut e = level_block(g.black);
                e.insert("gain".into(), json!(r2(g.gain)));
                Value::Object(e)
            })
            .collect();
        m.insert("by_gain".into(), Value::Array(list));
        m
    } else {
        let n = sweep.gains.len() as f64;
        let avg: [f64; 4] =
            std::array::from_fn(|c| sweep.gains.iter().map(|g| g.black[c]).sum::<f64>() / n);
        level_block(avg)
    };

    // Noise: the constant at the reference gain (unity gain's scale: read noise over √gain),
    // and per gain where there are several.
    let noise_at = |gain: f64, read: f64| r2(read * scale / gain.max(1e-9).sqrt());
    let mut noise = Map::new();
    let reference = sweep.gains.first().map(|g| (g.gain, g.read_noise_all()));
    if let Some((g0, read0)) = reference {
        noise.insert("reference_constant".into(), json!(noise_at(g0, read0)));
        if sweep.gains.len() >= 2 {
            let list: Vec<Value> = sweep
                .gains
                .iter()
                .map(|g| {
                    json!({
                        "gain": r2(g.gain),
                        "reference_constant": noise_at(g.gain, g.read_noise_all()),
                    })
                })
                .collect();
            noise.insert("by_gain".into(), Value::Array(list));
        }
    }

    let mut algorithms = vec![json!({ "rpi.black_level": Value::Object(black) })];
    if !noise.is_empty() {
        algorithms.push(json!({ "rpi.noise": Value::Object(noise) }));
    }

    let (temp_lo, temp_hi) = sweep
        .captures
        .iter()
        .filter_map(|c| c.temperature)
        .fold(None, |acc: Option<(f64, f64)>, (lo, hi)| match acc {
            None => Some((lo, hi)),
            Some((a, b)) => Some((a.min(lo), b.max(hi))),
        })
        .unzip();

    let mut doc = Map::new();
    doc.insert("overlay_version".into(), json!(OVERLAY_VERSION));
    doc.insert("sensor".into(), json!(prov.sensor));
    if let Some(u) = &prov.unit {
        doc.insert("unit".into(), json!(u));
    }
    doc.insert("date".into(), json!(prov.date));
    doc.insert("styx_commit".into(), json!(prov.styx_commit));
    if let (Some(lo), Some(hi)) = (temp_lo, temp_hi) {
        doc.insert(
            "temperature_c".into(),
            json!({ "min": r2(lo), "max": r2(hi) }),
        );
    }
    doc.insert(
        "source".into(),
        json!(format!(
            "styx-calibrate dark: {} captures, {} gains, {}-bit raw",
            sweep.captures.len(),
            sweep.gains.len(),
            first.bits
        )),
    );
    doc.insert(
        "units".into(),
        json!("black levels and noise constants on the 16-bit scale (raw x 2^(16 - bits)); exposure in microseconds"),
    );
    doc.insert("algorithms".into(), Value::Array(algorithms));
    if !defects.is_empty() {
        let mut pts: Vec<(usize, usize)> = defects.iter().map(|d| (d.x, d.y)).collect();
        pts.sort_by_key(|&(x, y)| (y, x));
        doc.insert(
            "defect_pixels".into(),
            Value::Array(pts.into_iter().map(|(x, y)| json!([x, y])).collect()),
        );
    }
    let measured: Vec<Value> = sweep
        .gains
        .iter()
        .map(|g| {
            json!({
                "gain": r2(g.gain),
                "captures": g.captures,
                "black_raw_dn": g.black.map(r2),
                "read_noise_raw_dn": g.read_noise.map(r2),
            })
        })
        .collect();
    let dark: Vec<Value> = sweep
        .dark_current
        .iter()
        .map(|d| {
            json!({
                "gain": r2(d.gain),
                "exposures": d.exposures,
                "dn_per_s": (d.slope_per_s * 1000.0).round() / 1000.0,
            })
        })
        .collect();
    let mut m = Map::new();
    m.insert("gains".into(), Value::Array(measured));
    m.insert("dark_current".into(), Value::Array(dark));
    if let Some((c, s)) = sweep.read_noise_fit {
        m.insert(
            "read_noise_fit_raw_dn".into(),
            json!({ "constant": r2(c), "slope_per_gain": r2(s) }),
        );
    }
    doc.insert("measured".into(), Value::Object(m));

    let value = Value::Object(doc);
    validate(&value)?;
    Ok(value)
}

/// The base the overlay is checked against: a Raspberry Pi tuning with a noise profile (the
/// overlay sets its constants and keeps the tuning's slope; a tuning's `rpi.noise` needs one).
pub const VALIDATION_BASE: &str = r#"{"version": 2.0, "algorithms": [{"rpi.noise": {"reference_constant": 0, "reference_slope": 3}}]}"#;

/// The overlay parses in Styx: merged over [`VALIDATION_BASE`], its blocks convert and the
/// result validates.
pub fn validate(overlay: &Value) -> Result<()> {
    let text = overlay.to_string();
    styx_algo::Tuning::from_rpi_json_str_with_overlay(VALIDATION_BASE, &text)
        .map(|_| ())
        .map_err(|e| Error(format!("the overlay does not parse in Styx: {e}")))
}

/// Today's date, `YYYY-MM-DD` (UTC).
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil(secs / 86_400)
}

/// `days` since 1970-01-01 as `YYYY-MM-DD` (the proleptic Gregorian calendar).
pub fn civil(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), "1970-01-01");
        assert_eq!(civil(20_000), "2024-10-04");
        assert_eq!(civil(20_738), "2026-10-12");
    }
}
