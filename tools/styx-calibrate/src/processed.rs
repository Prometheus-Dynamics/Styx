//! `check-processed`: the before and after metric for a processed (grey, Y8 from the ISP's NV12)
//! dark capture, `frames_<W>x<H>_gray.raw`.
//!
//! It reports the fraction of pixels that are exactly 0 (clipped black, which a correct black
//! level keeps at zero at every gain), the mean, and the temporal noise (the per-pixel temporal
//! standard deviation, averaged, and its 99th percentile).

use std::path::{Path, PathBuf};

use crate::raw::{self, Error, Result, err, read_meta};

/// Statistics of one processed dark capture.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessedStats {
    /// The directory.
    pub dir: PathBuf,
    /// Frames.
    pub frames: usize,
    /// Width and height.
    pub size: (usize, usize),
    /// Exposure time, microseconds (from frames.csv, if present).
    pub exposure_us: Option<f64>,
    /// Analogue gain (from frames.csv, if present).
    pub analog_gain: Option<f64>,
    /// Fraction of samples that are 0.
    pub clipped: f64,
    /// Mean sample value.
    pub mean: f64,
    /// Mean over pixels of the temporal standard deviation.
    pub temporal_mean: f64,
    /// 99th percentile over pixels of the temporal standard deviation.
    pub temporal_p99: f64,
}

/// The processed raw dump in `dir`: the `frames_*_gray.raw` file.
fn find_gray(dir: &Path) -> Result<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("frames_") && n.contains("gray") && n.ends_with(".raw")
            })
        })
        .collect();
    found.sort();
    match found.len() {
        0 => err(format!("{}: no frames_<W>x<H>_gray.raw", dir.display())),
        1 => Ok(found.remove(0)),
        _ => err(format!("{}: more than one grey dump", dir.display())),
    }
}

/// Analyses one processed grey capture directory.
pub fn check(dir: &Path) -> Result<ProcessedStats> {
    let file = find_gray(dir)?;
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let meta = read_meta(dir)?;
    let size = name
        .strip_prefix("frames_")
        .and_then(|s| s.split_once('_'))
        .and_then(|(wh, _)| wh.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
        .or_else(|| {
            let w = meta.get("width")?.parse().ok()?;
            let h = meta.get("height")?.parse().ok()?;
            Some((w, h))
        })
        .ok_or_else(|| Error(format!("{}: no size", dir.display())))?;
    let data = std::fs::read(&file)?;
    let settings = raw::csv_settings(dir)?;
    stats(dir, size, &data, settings)
}

/// The statistics of grey frames held in `data` (`size` each, back to back), with the mean
/// exposure and gain of the frames when known.
pub fn stats(
    dir: &Path,
    size: (usize, usize),
    data: &[u8],
    settings: Option<(f64, f64)>,
) -> Result<ProcessedStats> {
    let (w, h) = size;
    let frame = w * h;
    if frame == 0 || data.len() < frame {
        return err(format!("{}: too small for {w}x{h}", dir.display()));
    }
    let frames = data.len() / frame;
    if frames < 2 {
        return err(format!("{}: needs at least 2 frames", dir.display()));
    }
    let n = frames as f64;
    // Per-pixel temporal mean and variance, Welford over the frames.
    let mut mean = vec![0f64; frame];
    let mut m2 = vec![0f64; frame];
    let mut zeros = 0u64;
    let mut sum = 0f64;
    for f in 0..frames {
        let k = (f + 1) as f64;
        for (i, &b) in data[f * frame..(f + 1) * frame].iter().enumerate() {
            let v = f64::from(b);
            if b == 0 {
                zeros += 1;
            }
            sum += v;
            let d = v - mean[i];
            mean[i] += d / k;
            m2[i] += d * (v - mean[i]);
        }
    }
    let mut sigma: Vec<f64> = m2.iter().map(|m| (m / (n - 1.0)).sqrt()).collect();
    let temporal_mean = sigma.iter().sum::<f64>() / frame as f64;
    sigma.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p99 = sigma[((frame as f64 * 0.99) as usize).min(frame - 1)];
    let total = (frame * frames) as f64;
    let (exposure_us, analog_gain) = settings.map_or((None, None), |(e, g)| (Some(e), Some(g)));
    Ok(ProcessedStats {
        dir: dir.to_path_buf(),
        frames,
        size,
        exposure_us,
        analog_gain,
        clipped: zeros as f64 / total,
        mean: sum / total,
        temporal_mean,
        temporal_p99: p99,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_clipped_zeros_and_temporal_noise() {
        // Two pixels, three frames: pixel 0 constant 0, pixel 1 has the samples 2, 4, 2.
        let data: Vec<u8> = [[0u8, 2], [0, 4], [0, 2]]
            .iter()
            .flatten()
            .copied()
            .collect();
        let s = stats(Path::new("."), (2, 1), &data, Some((8000.0, 1.0))).unwrap();
        assert_eq!(s.frames, 3);
        assert!(
            (s.clipped - 0.5).abs() < 1e-12,
            "3 of 6 samples are 0: {}",
            s.clipped
        );
        assert!((s.mean - 8.0 / 6.0).abs() < 1e-12);
        assert_eq!((s.exposure_us, s.analog_gain), (Some(8000.0), Some(1.0)));
        // The sample variance of 2, 4, 2 is 4/3; pixel 0 has none.
        let s1 = (4.0f64 / 3.0).sqrt();
        assert!(
            (s.temporal_mean - s1 / 2.0).abs() < 1e-9,
            "{}",
            s.temporal_mean
        );
        assert!((s.temporal_p99 - s1).abs() < 1e-9);
    }
}
