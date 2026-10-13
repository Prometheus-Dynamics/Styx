//! Reading a raw capture directory: the frames, and what `frames.csv` and `meta.txt` say about
//! them.
//!
//! A capture is a directory holding one raw dump in this format:
//!
//! * `frames_<W>x<H>_bggr16.raw` (any name starting `frames_` and ending `.raw`, not `gray`):
//!   16-bit little-endian samples, the 10-bit values in the low bits, frames back to back, rows
//!   top to bottom, no padding;
//! * `frames.csv`: one row per frame, a header with at least the exposure and the analogue
//!   gain (`exposure_us`, `analog_gain`); a temperature column (`temperature`) is used if
//!   present;
//! * `meta.txt`: `key=value` lines. `width`, `height` and `cfa` (for example `BGGR`) and
//!   `bit_depth` (default 10) are read; the size and CFA may also come from the file name or
//!   from the caller ([`Overrides`]).
//!
//! The reader is tolerant: keys are matched without case, and a missing `meta.txt` is fine
//! when the size and CFA are given or in the file name.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// A calibration error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Shorthand for an [`Error`].
pub fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

/// One colour of the Bayer pattern, with the two greens told apart by the colour of their row
/// (Gr shares a row with red, Gb with blue), as the calibration reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Channel {
    /// Red.
    R,
    /// Green on the red row.
    Gr,
    /// Green on the blue row.
    Gb,
    /// Blue.
    B,
}

impl Channel {
    /// All four, in sensor order (the index used by the statistics).
    pub const ALL: [Channel; 4] = [Channel::R, Channel::Gr, Channel::Gb, Channel::B];

    /// The index in [`Channel::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }

    /// The name used in reports and overlays.
    pub fn name(self) -> &'static str {
        match self {
            Channel::R => "R",
            Channel::Gr => "Gr",
            Channel::Gb => "Gb",
            Channel::B => "B",
        }
    }
}

/// A Bayer colour filter array: the channel at each 2 × 2 position, indexed `[y & 1][x & 1]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cfa(pub [[Channel; 2]; 2]);

impl Cfa {
    /// Parses four letters, top-left, top-right, bottom-left, bottom-right (`BGGR`, `RGGB`,
    /// `GRBG`, `GBRG`, in any case).
    pub fn parse(text: &str) -> Result<Cfa> {
        let letters: Vec<char> = text.trim().to_ascii_uppercase().chars().collect();
        if letters.len() != 4 || !letters.iter().all(|c| matches!(c, 'R' | 'G' | 'B')) {
            return err(format!("cfa must be four of R, G, B (got {text:?})"));
        }
        let mut out = [[Channel::R; 2]; 2];
        for py in 0..2 {
            // The colours of this row decide which green is which.
            let row = &letters[py * 2..py * 2 + 2];
            let red_row = row.contains(&'R');
            for px in 0..2 {
                out[py][px] = match letters[py * 2 + px] {
                    'R' => Channel::R,
                    'B' => Channel::B,
                    _ if red_row => Channel::Gr,
                    _ => Channel::Gb,
                };
            }
        }
        let reds = letters.iter().filter(|c| **c == 'R').count();
        let blues = letters.iter().filter(|c| **c == 'B').count();
        if reds != 1 || blues != 1 {
            return err(format!("cfa must hold one R and one B (got {text:?})"));
        }
        Ok(Cfa(out))
    }

    /// The channel of the sample at `(x, y)`.
    pub fn channel(&self, x: usize, y: usize) -> Channel {
        self.0[y & 1][x & 1]
    }
}

/// Values from the caller that take precedence over the capture's own files.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    /// The CFA order, for example `BGGR`.
    pub cfa: Option<String>,
    /// Width and height.
    pub size: Option<(usize, usize)>,
    /// Bits per sample.
    pub bits: Option<u32>,
}

/// One row of `frames.csv`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameRow {
    /// Exposure time, microseconds.
    pub exposure_us: f64,
    /// Analogue gain.
    pub analog_gain: f64,
    /// Temperature, degrees Celsius, when the capture has it.
    pub temperature: Option<f64>,
}

/// A raw capture directory, opened.
#[derive(Debug)]
pub struct RawCapture {
    /// The directory.
    pub dir: PathBuf,
    /// Width, samples.
    pub width: usize,
    /// Height, rows.
    pub height: usize,
    /// The colour filter array.
    pub cfa: Cfa,
    /// Significant bits per sample (10 for a 10-bit sensor).
    pub bits: u32,
    /// Frames (the raw file's, or the csv's where it has fewer).
    pub frames: usize,
    /// One row per frame where the csv has them (may be shorter than `frames`).
    pub rows: Vec<FrameRow>,
    /// `meta.txt`, with lower-case keys.
    pub meta: BTreeMap<String, String>,
    data: Vec<u8>,
}

impl RawCapture {
    /// A capture held in memory, for tests and callers that have the samples already:
    /// `samples` holds whole frames of `width * height` values, back to back (each below
    /// `2^bits`), and `rows` is one row per frame (or none).
    pub fn from_samples(
        dir: PathBuf,
        (width, height): (usize, usize),
        cfa: Cfa,
        bits: u32,
        samples: &[u16],
        rows: Vec<FrameRow>,
    ) -> Self {
        let frames = samples.len() / (width * height);
        let data = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
        Self {
            dir,
            width,
            height,
            cfa,
            bits,
            frames,
            rows,
            meta: BTreeMap::new(),
            data,
        }
    }

    /// Frame `i`'s samples, as `width * height` little-endian `u16`s.
    pub fn frame_bytes(&self, i: usize) -> &[u8] {
        let n = self.width * self.height * 2;
        &self.data[i * n..(i + 1) * n]
    }

    /// The sample at `(x, y)` of frame `i`.
    pub fn sample(&self, i: usize, x: usize, y: usize) -> u16 {
        let o = (y * self.width + x) * 2;
        let b = &self.frame_bytes(i)[o..o + 2];
        u16::from_le_bytes([b[0], b[1]])
    }

    /// The mean exposure and gain over the frames, and whether they stayed within 1%.
    pub fn settings(&self) -> Result<Settings> {
        let rows = &self.rows[..self.rows.len().min(self.frames)];
        if rows.is_empty() {
            let (e, g) = (
                self.meta.get("requested_exposure_us"),
                self.meta.get("requested_gain"),
            );
            let (Some(e), Some(g)) = (
                e.and_then(|v| v.parse().ok()),
                g.and_then(|v| v.parse().ok()),
            ) else {
                return err(format!(
                    "{}: no exposure and gain (frames.csv, or requested_exposure_us and requested_gain in meta.txt)",
                    self.dir.display()
                ));
            };
            return Ok(Settings {
                exposure_us: e,
                analog_gain: g,
                steady: true,
            });
        }
        let mean = |f: fn(&FrameRow) -> f64| rows.iter().map(f).sum::<f64>() / rows.len() as f64;
        let spread = |f: fn(&FrameRow) -> f64| {
            let (lo, hi) = rows
                .iter()
                .map(f)
                .fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(v), h.max(v)));
            hi - lo <= 0.01 * hi.abs().max(1e-9)
        };
        Ok(Settings {
            exposure_us: mean(|r| r.exposure_us),
            analog_gain: mean(|r| r.analog_gain),
            steady: spread(|r| r.exposure_us) && spread(|r| r.analog_gain),
        })
    }

    /// The temperature range over the frames, when the csv has it.
    pub fn temperature_range(&self) -> Option<(f64, f64)> {
        let t: Vec<f64> = self.rows.iter().filter_map(|r| r.temperature).collect();
        let lo = t.iter().copied().reduce(f64::min)?;
        let hi = t.iter().copied().reduce(f64::max)?;
        Some((lo, hi))
    }
}

/// The exposure and gain a capture was taken at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// Exposure time, microseconds.
    pub exposure_us: f64,
    /// Analogue gain.
    pub analog_gain: f64,
    /// Whether every frame had the same settings (within 1%).
    pub steady: bool,
}

/// Parses `meta.txt`: `key=value` lines, keys lower-cased, `#` comments and blank lines
/// skipped, missing file gives an empty map.
pub fn read_meta(dir: &Path) -> Result<BTreeMap<String, String>> {
    let path = dir.join("meta.txt");
    let mut out = BTreeMap::new();
    if !path.is_file() {
        return Ok(out);
    }
    for line in std::fs::read_to_string(&path)?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Ok(out)
}

/// `frames_<W>x<H>_<fmt>.raw` as `(W, H, fmt)`.
fn name_size(file: &str) -> Option<(usize, usize, String)> {
    let stem = file.strip_prefix("frames_")?.strip_suffix(".raw")?;
    let (size, fmt) = stem.split_once('_')?;
    let (w, h) = size.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, fmt.to_ascii_lowercase()))
}

/// The raw dump in `dir`: the `frames_*.raw` file that is not grey (a processed capture).
pub fn find_raw(dir: &Path) -> Result<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("frames_") && n.ends_with(".raw") && !n.contains("gray")
            })
        })
        .collect();
    found.sort();
    match found.len() {
        0 => err(format!(
            "{}: no raw dump (frames_<W>x<H>_bggr16.raw); a processed grey capture is for check-processed",
            dir.display()
        )),
        1 => Ok(found.remove(0)),
        _ => err(format!("{}: more than one raw dump", dir.display())),
    }
}

/// Reads the frames.csv rows the named columns give.
fn read_rows(dir: &Path) -> Result<Vec<FrameRow>> {
    let path = dir.join("frames.csv");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return Ok(Vec::new());
    };
    let names: Vec<String> = header
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    let col = |names_: &[&str]| names.iter().position(|n| names_.contains(&n.as_str()));
    let (Some(exp), Some(gain)) = (
        col(&["exposure_us", "exposure"]),
        col(&["analog_gain", "analogue_gain", "gain"]),
    ) else {
        return err(format!(
            "{}: needs exposure_us and analog_gain columns",
            path.display()
        ));
    };
    let temp = col(&["temperature", "temperature_c", "temp_c"]);
    // The board's sensor-side temperature in millidegrees, as the capture tool writes it.
    let temp_mdeg = col(&["board_temp_mdegc"]);
    let mut rows = Vec::new();
    for (n, line) in lines.enumerate() {
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        let num = |i: usize| -> Result<f64> {
            f.get(i).and_then(|v| v.parse::<f64>().ok()).ok_or_else(|| {
                Error(format!(
                    "{}: row {} has no number in column {i}",
                    path.display(),
                    n + 1
                ))
            })
        };
        rows.push(FrameRow {
            exposure_us: num(exp)?,
            analog_gain: num(gain)?,
            temperature: match (temp, temp_mdeg) {
                (Some(t), _) => f.get(t).and_then(|v| v.parse().ok()),
                (None, Some(t)) => f
                    .get(t)
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|v| v / 1000.0),
                (None, None) => None,
            },
        });
    }
    Ok(rows)
}

/// Opens a raw capture directory (see the [module documentation](self)).
pub fn open(dir: &Path, over: &Overrides) -> Result<RawCapture> {
    let raw = find_raw(dir)?;
    let file = raw
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let meta = read_meta(dir)?;
    let from_name = name_size(&file);
    let num = |k: &str| meta.get(k).and_then(|v| v.parse::<usize>().ok());
    let (width, height) = match over.size {
        Some(s) => s,
        None => match (num("width"), num("height"), &from_name) {
            (Some(w), Some(h), _) => (w, h),
            (_, _, Some((w, h, _))) => (*w, *h),
            _ => {
                return err(format!(
                    "{}: no size (meta.txt width/height, or the file name)",
                    dir.display()
                ));
            }
        },
    };
    let cfa_text = over
        .cfa
        .clone()
        .or_else(|| {
            ["cfa", "cfa_order", "bayer", "cfa_pattern"]
                .iter()
                .find_map(|k| meta.get(*k).cloned())
        })
        .ok_or_else(|| {
            Error(format!(
                "{}: no CFA order (meta.txt cfa=, or --cfa)",
                dir.display()
            ))
        })?;
    // The order is the first word: `BGGR (row 0: ...)` in some files.
    let cfa_text = cfa_text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    let cfa = Cfa::parse(&cfa_text)?;
    let bits = over
        .bits
        .or_else(|| meta.get("bit_depth").and_then(|v| v.parse().ok()))
        .unwrap_or(10);
    if !(1..=16).contains(&bits) {
        return err(format!("bit_depth {bits} is out of range"));
    }
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return err(format!(
            "{width}x{height}: the size must be even and non-zero"
        ));
    }
    let data = std::fs::read(&raw)?;
    let frame_len = width * height * 2;
    let mut frames = data.len() / frame_len;
    if data.len() % frame_len != 0 {
        eprintln!(
            "warning: {}: {} bytes left over after {frames} frames of {width}x{height}",
            raw.display(),
            data.len() % frame_len
        );
    }
    let rows = read_rows(dir)?;
    if !rows.is_empty() && rows.len() != frames {
        eprintln!(
            "warning: {}: {} csv rows for {frames} frames; using the smaller",
            dir.display(),
            rows.len()
        );
        frames = frames.min(rows.len());
    }
    let mut data = data;
    data.truncate(frames * frame_len);
    Ok(RawCapture {
        dir: dir.to_path_buf(),
        width,
        height,
        cfa,
        bits,
        frames,
        rows,
        meta,
        data,
    })
}

/// The sensor name a capture names: `sensor=` or `camera=` in meta.txt (the first word of the
/// latter, `ov9782` in `camera=ov9782 (kernel driver ...)`).
pub fn sensor_name(meta: &BTreeMap<String, String>) -> Option<String> {
    if let Some(s) = meta.get("sensor") {
        return Some(s.split_whitespace().next()?.to_ascii_lowercase());
    }
    let c = meta.get("camera")?;
    let word: String = c
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .collect();
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
}

/// The mean exposure (µs) and analogue gain of a capture's frames.csv, when it has rows.
pub fn csv_settings(dir: &Path) -> Result<Option<(f64, f64)>> {
    let rows = read_rows(dir)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let n = rows.len() as f64;
    let e = rows.iter().map(|r| r.exposure_us).sum::<f64>() / n;
    let g = rows.iter().map(|r| r.analog_gain).sum::<f64>() / n;
    Ok(Some((e, g)))
}

/// The unit's identity a capture names: `unit=` or `serial=` in meta.txt.
pub fn unit_name(meta: &BTreeMap<String, String>) -> Option<String> {
    meta.get("unit").or_else(|| meta.get("serial")).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfa_tells_the_greens_apart_by_their_row() {
        let bggr = Cfa::parse("BGGR").unwrap();
        assert_eq!(bggr.channel(0, 0), Channel::B);
        assert_eq!(bggr.channel(1, 0), Channel::Gb, "green on the blue row");
        assert_eq!(bggr.channel(0, 1), Channel::Gr, "green on the red row");
        assert_eq!(bggr.channel(1, 1), Channel::R);
        assert_eq!(bggr.channel(2, 2), Channel::B, "the pattern repeats");
        let rggb = Cfa::parse("rggb").unwrap();
        assert_eq!(rggb.channel(0, 0), Channel::R);
        assert_eq!(rggb.channel(1, 0), Channel::Gr);
        assert_eq!(rggb.channel(0, 1), Channel::Gb);
        assert!(Cfa::parse("BGGX").is_err());
        assert!(Cfa::parse("BBGR").is_err(), "one red and one blue");
    }

    #[test]
    fn names_and_sizes_from_file_names() {
        assert_eq!(
            name_size("frames_1280x800_bggr16.raw"),
            Some((1280, 800, "bggr16".into()))
        );
        assert_eq!(name_size("frames.raw"), None);
        assert_eq!(
            sensor_name(&BTreeMap::from([(
                "camera".into(),
                "OV9782 (kernel driver i2c 10-0060)".into()
            )])),
            Some("ov9782".into())
        );
    }
}
