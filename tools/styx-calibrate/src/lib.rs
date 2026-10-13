//! styx-calibrate: a sensor's black level, noise and defect profile from raw dark-frame
//! captures, written as the tuning overlay Styx loads over the sensor's builtin tuning.
//!
//! The library is what the CLI (`styx-calibrate`) and other tools use. A dark sweep is
//! [`run_dark`]: the capture directories in, a Markdown report and an overlay out. A processed
//! grey capture's before-and-after check is [`processed::check`]. The calibration flow is
//! documented in `docs/calibration.md`.

pub mod dark;
pub mod overlay;
pub mod processed;
pub mod raw;
pub mod report;
pub mod synth;

use std::path::PathBuf;

pub use raw::{Error, Result};

/// What a dark run is told about the unit and the build.
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Analysis options.
    pub dark: dark::DarkOptions,
    /// Overrides for the capture files (CFA, size, bit depth).
    pub raw: raw::Overrides,
    /// The sensor's name (the tuning's stem); from the captures when `None`.
    pub sensor: Option<String>,
    /// The unit's serial or name; from the captures when `None`.
    pub unit: Option<String>,
    /// The date, `YYYY-MM-DD`; today when `None`.
    pub date: Option<String>,
    /// The Styx commit; `unknown` when `None`.
    pub styx_commit: Option<String>,
    /// What the overlay carries beyond the defaults (see [`overlay::Choices`]).
    pub overlay: overlay::Choices,
}

/// The result of a dark run: the report, the overlay, and the sweep it came from.
#[derive(Debug)]
pub struct DarkRun {
    /// The Markdown report.
    pub report: String,
    /// The overlay (the JSON document Styx reads).
    pub overlay: serde_json::Value,
    /// The sensor the overlay is for.
    pub sensor: String,
    /// The sweep the report and overlay summarise.
    pub sweep: dark::Sweep,
    /// The defect pixels the overlay lists.
    pub defects: Vec<dark::DefectPixel>,
}

impl DarkRun {
    /// The overlay as the JSON text written to disk (pretty, with a final newline).
    pub fn overlay_text(&self) -> String {
        let mut s = serde_json::to_string_pretty(&self.overlay).unwrap_or_default();
        s.push('\n');
        s
    }
}

/// Analyses dark captures (one exposure and gain each, in `dirs`) and makes the report and the
/// tuning overlay. Defect pixels are the union over the captures (each listed once, at its
/// first capture in gain order).
pub fn run_dark(dirs: &[PathBuf], opts: &RunOptions) -> Result<DarkRun> {
    let sweep = dark::run(dirs, &opts.raw, &opts.dark)?;
    let first = &sweep.captures[0];
    let sensor = opts
        .sensor
        .clone()
        .or_else(|| first.sensor.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let unit = opts.unit.clone().or_else(|| first.unit.clone());
    let prov = overlay::Provenance {
        sensor: sensor.clone(),
        unit,
        date: opts.date.clone().unwrap_or_else(overlay::today),
        styx_commit: opts.styx_commit.clone().unwrap_or_else(|| "unknown".into()),
    };
    let mut defects: Vec<dark::DefectPixel> = Vec::new();
    for c in &sweep.captures {
        for d in &c.defects {
            if !defects.iter().any(|e| e.x == d.x && e.y == d.y) {
                defects.push(d.clone());
            }
        }
    }
    let overlay = overlay::build_with(&sweep, &prov, &defects, opts.overlay)?;
    let report = report::dark(&sweep, &prov, &defects);
    Ok(DarkRun {
        report,
        overlay,
        sensor,
        sweep,
        defects,
    })
}
