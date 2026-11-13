//! A calibration session: which captures show what.
//!
//! A session file (TOML) names each capture. Raw captures carry their own size, mosaic and
//! settings, since a raw file holds no metadata:
//!
//! ```toml
//! sensor = "fixture"            # names the outputs; optional
//! [[shot]]
//! kind = "dark"                 # lens covered (any exposures and gains)
//! file = "dark.raw"
//! width = 1280
//! height = 800
//! cfa = "rggb"
//! bits = 10
//! exposure_us = 10000
//! gain = 1
//! [[shot]]
//! kind = "flat"                 # diffuser / white wall filling the frame
//! ct = 3000
//! file = "flat_3000k.raw"
//! # ... the same raw fields
//! [[shot]]
//! kind = "macbeth"              # ColorChecker 24
//! ct = 5000
//! lux = 800                     # optional: makes this the lux reference
//! file = "macbeth_5000k.raw"
//! corners = [[210, 140], [1050, 150], [1040, 700], [205, 690]]  # optional, see below
//! ```
//!
//! A raw file is consecutive frames of packed little-endian 16-bit samples (`width × height`
//! each, the bit depth in `bits` says how many low bits are used).
//!
//! `corners` are the centres of the chart's corner patches in full-resolution pixels: dark
//! skin, bluish green, black, white (clockwise from top-left as printed), for when automatic
//! detection fails. Paths are relative to the session file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, TuneError};
use crate::input;
use crate::raw::{RawFrame, parse_cfa};

/// What a capture shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Lens covered: black level and hot pixels. Frames at several exposures of the same gain
    /// (lens uncovered) also work: the level is extrapolated to zero exposure.
    Dark,
    /// A uniform, evenly lit field (diffuser over the lens, or a white wall): lens shading.
    Flat,
    /// The ColorChecker 24: AWB curve, colour matrix, noise, lux.
    Macbeth,
    /// A grey card filling the centre of the frame: AWB curve point.
    Grey,
    /// Any static scene, several frames at fixed settings: temporal noise.
    Noise,
}

/// One entry of a session file.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShotSpec {
    /// What it shows.
    pub kind: Kind,
    /// The capture (a raw file).
    pub file: PathBuf,
    /// Samples across the raw file.
    pub width: usize,
    /// Rows of the raw file.
    pub height: usize,
    /// The mosaic's top-left quad: `rggb`, `grbg`, `gbrg` or `bggr`.
    pub cfa: String,
    /// Bits per sample.
    pub bits: u8,
    /// Colour temperature of the light, kelvin (flat, macbeth, grey).
    #[serde(default)]
    pub ct: Option<f64>,
    /// Illuminance at the chart, lux (one macbeth shot: the lux reference).
    #[serde(default)]
    pub lux: Option<f64>,
    /// Centres of the corner patches, full-resolution pixels (dark skin, bluish green, black,
    /// white).
    #[serde(default)]
    pub corners: Option<[[f64; 2]; 4]>,
    /// Frames to leave out at the start (default 1 when there are 3 or more: a sensor's first
    /// frame after a start can read a different black level).
    #[serde(default)]
    pub skip: Option<usize>,
    /// Use at most this many frames.
    #[serde(default)]
    pub frames: Option<usize>,
    /// Exposure of the frames, microseconds.
    pub exposure_us: f64,
    /// Analogue gain of the frames.
    pub gain: f64,
}

/// A session file.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFile {
    /// Sensor name (output file names).
    #[serde(default)]
    pub sensor: Option<String>,
    /// Free text kept in the tuning's description.
    #[serde(default)]
    pub description: Option<String>,
    /// The shots.
    #[serde(default, rename = "shot")]
    pub shots: Vec<ShotSpec>,
}

/// A loaded shot.
#[derive(Clone, Debug)]
pub struct Shot {
    /// File name, for reports.
    pub name: String,
    /// What it shows.
    pub kind: Kind,
    /// Light colour temperature.
    pub ct: Option<f64>,
    /// Illuminance.
    pub lux: Option<f64>,
    /// Manual chart corners.
    pub corners: Option<[[f64; 2]; 4]>,
    /// Frames used.
    pub frames: Vec<RawFrame>,
}

impl SessionFile {
    /// Parse a session file.
    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| TuneError::Session(e.to_string()))
    }

    /// Read a session file from disk.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| TuneError::io(path, e))?;
        Self::parse(&text)
    }

    /// Load every shot's frames, paths relative to `dir` (the session file's directory).
    pub fn load(&self, dir: &Path) -> Result<Vec<Shot>> {
        self.shots
            .iter()
            .map(|s| {
                let path = dir.join(&s.file);
                let cfa = parse_cfa(&s.cfa).ok_or_else(|| {
                    TuneError::Session(format!("{}: unknown cfa {:?}", path.display(), s.cfa))
                })?;
                let frames = input::read_raw(
                    &path,
                    &input::RawSpec {
                        width: s.width,
                        height: s.height,
                        cfa,
                        bits: s.bits,
                        exposure_us: s.exposure_us,
                        gain: s.gain,
                    },
                )?;
                let frames = select(frames, s);
                if frames.is_empty() {
                    return Err(TuneError::Session(format!("{}: no frames", path.display())));
                }
                if matches!(s.kind, Kind::Flat | Kind::Macbeth | Kind::Grey) && s.ct.is_none() {
                    return Err(TuneError::Session(format!(
                        "{}: a {:?} shot needs its colour temperature (ct)",
                        path.display(),
                        s.kind
                    )));
                }
                Ok(Shot {
                    name: s.file.display().to_string(),
                    kind: s.kind,
                    ct: s.ct,
                    lux: s.lux,
                    corners: s.corners,
                    frames,
                })
            })
            .collect()
    }
}

/// Frames to use: the first frames dropped, the most common exposure and gain kept (dark shots
/// keep every setting), at most `frames`.
fn select(mut frames: Vec<RawFrame>, s: &ShotSpec) -> Vec<RawFrame> {
    let skip = s.skip.unwrap_or(if frames.len() >= 3 { 1 } else { 0 });
    frames.drain(..skip.min(frames.len()));
    if s.kind != Kind::Dark && !frames.is_empty() {
        let key = |f: &RawFrame| {
            (
                (f.exposure_us * 10.0).round() as i64,
                (f.gain() * 1000.0).round() as i64,
            )
        };
        let mut counts: Vec<((i64, i64), usize)> = Vec::new();
        for f in &frames {
            match counts.iter_mut().find(|(k, _)| *k == key(f)) {
                Some((_, n)) => *n += 1,
                None => counts.push((key(f), 1)),
            }
        }
        let best = counts.iter().max_by_key(|(_, n)| *n).map(|(k, _)| *k);
        frames.retain(|f| Some(key(f)) == best);
    }
    if let Some(n) = s.frames {
        frames.truncate(n);
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_files_parse() {
        let s = SessionFile::parse(
            r#"
            sensor = "fixture"
            [[shot]]
            kind = "macbeth"
            file = "a.raw"
            width = 8
            height = 4
            cfa = "rggb"
            bits = 10
            ct = 5000
            lux = 800
            exposure_us = 1000
            gain = 1
            corners = [[1, 2], [3, 4], [5, 6], [7, 8]]
            "#,
        )
        .unwrap();
        assert_eq!(s.shots[0].kind, Kind::Macbeth);
        assert_eq!(s.shots[0].corners.unwrap()[3], [7.0, 8.0]);
        assert!(SessionFile::parse("[[shot]]\nkind = \"sunset\"\nfile = \"x\"").is_err());
        // A raw shot without its settings is refused.
        assert!(
            SessionFile::parse(
                "[[shot]]\nkind = \"dark\"\nfile = \"x.raw\"\nwidth = 1\nheight = 1\ncfa = \"rggb\"\nbits = 10\n"
            )
            .is_err()
        );
    }
}
