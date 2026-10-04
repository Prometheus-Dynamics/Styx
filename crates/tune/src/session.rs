//! A calibration session: which captures show what.
//!
//! Either a session file (TOML):
//!
//! ```toml
//! sensor = "ov9782"            # names the outputs; optional
//! [[shot]]
//! kind = "dark"                # lens covered (any exposures and gains)
//! file = "dark.mcap"
//! [[shot]]
//! kind = "flat"                # diffuser / white wall filling the frame
//! ct = 3000
//! file = "flat_3000k.dng"
//! [[shot]]
//! kind = "macbeth"             # ColorChecker 24
//! ct = 5000
//! lux = 800                    # optional: makes this the lux reference
//! file = "macbeth_5000k.mcap"
//! corners = [[210, 140], [1050, 150], [1040, 700], [205, 690]]  # optional, see below
//! ```
//!
//! or a directory named the way Raspberry Pi's `ctt` names its inputs: `dark*` or `black*`,
//! `alsc_<T>k*` or `flat_<T>k*` for flat fields, `<T>k_<L>l*` or `<T>k*` for charts (T kelvin,
//! L lux), `grey_<T>k*` for a grey card filling the centre, `noise*` for a static scene for the
//! noise profile.
//!
//! `exposure_us` and `gain` give the settings of frames whose file does not record them (DNGs
//! without EXIF, MCAP recordings of format 1); frames without known settings are left out of
//! the noise profile and the lux reference.
//!
//! `corners` are the centres of the chart's corner patches in full-resolution pixels: dark
//! skin, bluish green, black, white (clockwise from top-left as printed), for when automatic
//! detection fails. Paths are relative to the session file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, TuneError};
use crate::input::Loader;
use crate::raw::RawFrame;

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
    /// The capture.
    pub file: PathBuf,
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
    /// Exposure of frames whose file does not record it, microseconds.
    #[serde(default)]
    pub exposure_us: Option<f64>,
    /// Analogue gain of frames whose file does not record their exposure and gain.
    #[serde(default)]
    pub gain: Option<f64>,
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

    /// A session from a directory of files named as `ctt` names them.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| TuneError::io(dir, e))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "dng" | "mcap" | "jsonl" | "tif" | "tiff"
                )
            })
            .collect();
        names.sort();
        let mut shots = Vec::new();
        for p in names {
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if let Some(spec) =
                spec_from_name(&stem, p.file_name().map(PathBuf::from).unwrap_or_default())
            {
                shots.push(spec);
            }
        }
        if shots.is_empty() {
            return Err(TuneError::Session(format!(
                "{}: no captures named dark*, alsc_<T>k*, flat_<T>k*, <T>k[_<L>l]*, grey_<T>k*, noise*",
                dir.display()
            )));
        }
        Ok(Self {
            shots,
            ..Self::default()
        })
    }

    /// Load every shot's frames, paths relative to `dir`.
    pub fn load(&self, dir: &Path, loader: &Loader) -> Result<Vec<Shot>> {
        self.shots
            .iter()
            .map(|s| {
                let path = dir.join(&s.file);
                let frames = select(loader.load(&path)?, s);
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

/// The shot a `ctt`-style file name describes.
fn spec_from_name(stem: &str, file: PathBuf) -> Option<ShotSpec> {
    let number = |s: &str, suffix: char| -> Option<f64> {
        let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
        (!digits.is_empty() && s[digits.len()..].starts_with(suffix))
            .then(|| digits.parse().ok())
            .flatten()
    };
    let spec = |kind, ct, lux| ShotSpec {
        kind,
        file: file.clone(),
        ct,
        lux,
        corners: None,
        skip: None,
        frames: None,
        exposure_us: None,
        gain: None,
    };
    if stem.starts_with("dark") || stem.starts_with("black") {
        return Some(spec(Kind::Dark, None, None));
    }
    if stem.starts_with("noise") {
        return Some(spec(Kind::Noise, None, None));
    }
    for (prefix, kind) in [
        ("alsc_", Kind::Flat),
        ("flat_", Kind::Flat),
        ("grey_", Kind::Grey),
        ("gray_", Kind::Grey),
    ] {
        if let Some(rest) = stem.strip_prefix(prefix) {
            return number(rest, 'k').map(|ct| spec(kind, Some(ct), None));
        }
    }
    let ct = number(stem, 'k')?;
    let rest = stem.split_once('_').map(|(_, r)| r).unwrap_or("");
    Some(spec(Kind::Macbeth, Some(ct), number(rest, 'l')))
}

/// Frames to use: settings filled in where the file has none, the first frames dropped, the most
/// common exposure and gain kept (dark shots keep every setting), at most `frames`.
fn select(mut frames: Vec<RawFrame>, s: &ShotSpec) -> Vec<RawFrame> {
    for f in frames.iter_mut().filter(|f| f.exposure_us <= 0.0) {
        if let Some(e) = s.exposure_us {
            f.exposure_us = e;
        }
        if let Some(g) = s.gain {
            f.analogue_gain = g;
            f.digital_gain = 1.0;
        }
    }
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
    fn ctt_names() {
        let k = |s: &str| spec_from_name(s, PathBuf::from(s)).map(|s| (s.kind, s.ct, s.lux));
        assert_eq!(k("dark_gain4"), Some((Kind::Dark, None, None)));
        assert_eq!(k("alsc_3000k_1"), Some((Kind::Flat, Some(3000.0), None)));
        assert_eq!(
            k("5000k_800l"),
            Some((Kind::Macbeth, Some(5000.0), Some(800.0)))
        );
        assert_eq!(k("2850k"), Some((Kind::Macbeth, Some(2850.0), None)));
        assert_eq!(k("grey_6500k"), Some((Kind::Grey, Some(6500.0), None)));
        assert_eq!(k("holiday"), None);
    }

    #[test]
    fn session_files_parse() {
        let s = SessionFile::parse(
            r#"
            sensor = "ov9782"
            [[shot]]
            kind = "macbeth"
            file = "a.mcap"
            ct = 5000
            lux = 800
            corners = [[1, 2], [3, 4], [5, 6], [7, 8]]
            "#,
        )
        .unwrap();
        assert_eq!(s.shots[0].kind, Kind::Macbeth);
        assert_eq!(s.shots[0].corners.unwrap()[3], [7.0, 8.0]);
        assert!(SessionFile::parse("[[shot]]\nkind = \"sunset\"\nfile = \"x\"").is_err());
    }
}
