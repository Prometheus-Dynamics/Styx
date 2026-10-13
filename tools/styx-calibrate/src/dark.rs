//! Dark-frame analysis: one capture (one exposure and gain) and the sweep across captures.
//!
//! Per capture, for each CFA channel (R, Gr, Gb, B):
//!
//! * the black level, as the mean of every sample and as the robust median of the per-pixel
//!   temporal means (defect pixels left out);
//! * read noise: the temporal standard deviation of each pixel, averaged over the channel;
//! * DSNU / fixed-pattern noise: the spatial standard deviation of the per-pixel temporal means,
//!   and the row and column fixed-pattern noise (the spread of each row's and column's mean);
//! * row and column temporal noise: the temporal standard deviation of each row's and column's
//!   mean, averaged;
//! * defect pixels: hot ones (temporal mean above the black level by `k` times the channel's
//!   read noise), stuck ones (no temporal variation), and noisy ones (a temporal deviation far
//!   above the channel's).
//!
//! Across captures: black level per gain, read noise per gain (fitted as a line in gain), the
//! noise-model constant per gain (read noise over √gain, the scaling the denoise code applies),
//! and dark current per gain (the slope of the mean signal against exposure time).

use std::path::PathBuf;

use crate::raw::{self, Channel, Overrides, RawCapture, Settings};

/// Histogram bins: every 16-bit sample value.
const HIST_BINS: usize = 1 << 16;

/// Options of the analysis.
#[derive(Clone, Debug, PartialEq)]
pub struct DarkOptions {
    /// A pixel is hot when its temporal mean is above the black level by more than this many
    /// times the channel's read noise.
    pub hot_k: f64,
    /// Black level change across gains (raw DN) above which the levels are written per gain.
    pub black_move: f64,
}

impl Default for DarkOptions {
    fn default() -> Self {
        Self {
            hot_k: 6.0,
            black_move: 0.5,
        }
    }
}

/// Why a pixel is listed as a defect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefectReason {
    /// The temporal mean is far above black.
    Hot,
    /// No temporal variation, or far less than the channel's.
    Stuck,
    /// Far more temporal variation than the channel's.
    Noisy,
}

impl DefectReason {
    /// The name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            DefectReason::Hot => "hot",
            DefectReason::Stuck => "stuck",
            DefectReason::Noisy => "noisy",
        }
    }
}

/// A defect pixel, in the sensor's raw coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct DefectPixel {
    /// Column.
    pub x: usize,
    /// Row.
    pub y: usize,
    /// Its CFA channel.
    pub channel: Channel,
    /// Why it is listed.
    pub reason: DefectReason,
    /// Its temporal mean (raw DN).
    pub mean: f64,
    /// Its temporal standard deviation (raw DN).
    pub sigma: f64,
}

/// One CFA channel's statistics for one capture (raw DN).
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelStats {
    /// The channel.
    pub channel: Channel,
    /// Mean of every sample of the channel, defects included.
    pub black_mean: f64,
    /// Median of the per-pixel temporal means, defects left out: the black level to use.
    pub black_median: f64,
    /// Mean of the per-pixel temporal standard deviations, defects left out.
    pub read_noise: f64,
    /// Spatial standard deviation of the per-pixel temporal means, defects left out.
    pub dsnu: f64,
    /// Spread of the per-row temporal means (the row fixed pattern), defects left out.
    pub row_fpn: f64,
    /// Spread of the per-column temporal means (the column fixed pattern), defects left out.
    pub col_fpn: f64,
    /// Mean over rows of the temporal standard deviation of the row mean.
    pub row_noise: f64,
    /// Mean over columns of the temporal standard deviation of the column mean.
    pub col_noise: f64,
    /// Fraction of the channel's samples that are exactly 0 (clipped black).
    pub clipped_zero: f64,
}

/// One capture (one exposure and gain) analysed.
#[derive(Clone, Debug, PartialEq)]
pub struct DarkCapture {
    /// The directory.
    pub dir: PathBuf,
    /// Exposure time, microseconds (mean over the frames).
    pub exposure_us: f64,
    /// Analogue gain (mean over the frames).
    pub analog_gain: f64,
    /// Whether the settings held over every frame.
    pub steady: bool,
    /// Frames analysed.
    pub frames: usize,
    /// Significant bits per sample.
    pub bits: u32,
    /// The statistics of R, Gr, Gb and B, in that order.
    pub channels: [ChannelStats; 4],
    /// Defect pixels, in raster order.
    pub defects: Vec<DefectPixel>,
    /// Temperature range over the frames, when recorded.
    pub temperature: Option<(f64, f64)>,
    /// The sensor's name, when the capture names it.
    pub sensor: Option<String>,
    /// The unit's name, when the capture names it.
    pub unit: Option<String>,
}

impl DarkCapture {
    /// The mean of every sample over the four channels (raw DN).
    pub fn mean_black(&self) -> f64 {
        self.channels.iter().map(|c| c.black_mean).sum::<f64>() / 4.0
    }

    /// The read noise averaged over the four channels (raw DN).
    pub fn read_noise(&self) -> f64 {
        self.channels.iter().map(|c| c.read_noise).sum::<f64>() / 4.0
    }

    /// The factor from raw samples to the 16-bit scale the back end and the tuning files use
    /// (64 for 10-bit).
    pub fn scale16(&self) -> f64 {
        65536.0 / f64::from(1u32 << self.bits)
    }
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Mean and population standard deviation.
fn mean_std(v: impl Iterator<Item = f64> + Clone) -> (f64, f64) {
    let n = v.clone().count();
    if n == 0 {
        return (f64::NAN, f64::NAN);
    }
    let m = v.clone().sum::<f64>() / n as f64;
    let var = v.map(|x| (x - m) * (x - m)).sum::<f64>() / n as f64;
    (m, var.sqrt())
}

/// Analyses one opened capture.
pub fn analyse(cap: &RawCapture, opts: &DarkOptions) -> raw::Result<DarkCapture> {
    let n = cap.frames;
    if n < 2 {
        return raw::err(format!(
            "{}: needs at least 2 frames for a temporal noise (got {n})",
            cap.dir.display()
        ));
    }
    let settings: Settings = cap.settings()?;
    let (w, h) = (cap.width, cap.height);
    let npx = w * h;
    let channel_of = |i: usize| cap.cfa.channel(i % w, i / w);

    // Pass 1: per-pixel temporal mean and variance (Welford), and the samples' histograms.
    let mut mean = vec![0f64; npx];
    let mut m2 = vec![0f64; npx];
    let mut hist = vec![0u64; 4 * HIST_BINS];
    for f in 0..n {
        let bytes = cap.frame_bytes(f);
        let k = (f + 1) as f64;
        for i in 0..npx {
            let v = f64::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]));
            let d = v - mean[i];
            mean[i] += d / k;
            m2[i] += d * (v - mean[i]);
            let c = channel_of(i).index();
            hist[c * HIST_BINS + (v as usize).min(HIST_BINS - 1)] += 1;
        }
    }
    let sigma: Vec<f64> = m2.iter().map(|m| (m / (n as f64 - 1.0)).sqrt()).collect();

    // The channels' typical values, from which the defects are judged.
    let mut per_channel: [Vec<usize>; 4] = Default::default();
    for i in 0..npx {
        per_channel[channel_of(i).index()].push(i);
    }
    // Per channel: the black level and the read noise (median σ). A real sensor's per-pixel σ
    // has a broad upper tail (the OV9782 dark run: 99th percentile 2.5 × the median), so a pixel
    // is "noisy" only far out in it, at four times the median.
    let mut typical = [(0.0f64, 0.0f64); 4];
    for c in 0..4 {
        let mut means: Vec<f64> = per_channel[c].iter().map(|&i| mean[i]).collect();
        let mut sig: Vec<f64> = per_channel[c].iter().map(|&i| sigma[i]).collect();
        typical[c] = (median(&mut means), median(&mut sig));
    }

    // Defect pixels.
    let mut defect = vec![false; npx];
    let mut defects = Vec::new();
    for i in 0..npx {
        let c = channel_of(i);
        let (black, sd) = typical[c.index()];
        let sd = sd.max(1e-3);
        let (m, s) = (mean[i], sigma[i]);
        let reason = if m - black > opts.hot_k * sd {
            Some(DefectReason::Hot)
        } else if s < 0.25 * sd {
            Some(DefectReason::Stuck)
        } else if s > 4.0 * sd {
            Some(DefectReason::Noisy)
        } else {
            None
        };
        if let Some(reason) = reason {
            defect[i] = true;
            defects.push(DefectPixel {
                x: i % w,
                y: i / w,
                channel: c,
                reason,
                mean: m,
                sigma: s,
            });
        }
    }

    // Pass 2: row and column temporal means over the good pixels.
    let mut row_cnt = vec![0usize; 2 * h];
    let mut col_cnt = vec![0usize; 2 * w];
    for (i, &bad) in defect.iter().enumerate() {
        if !bad {
            let (x, y) = (i % w, i / w);
            row_cnt[y * 2 + (x & 1)] += 1;
            col_cnt[x * 2 + (y & 1)] += 1;
        }
    }
    let mut row_mean = vec![0f64; 2 * h];
    let mut row_m2 = vec![0f64; 2 * h];
    let mut col_mean = vec![0f64; 2 * w];
    let mut col_m2 = vec![0f64; 2 * w];
    let mut row_sum = vec![0f64; 2 * h];
    let mut col_sum = vec![0f64; 2 * w];
    for f in 0..n {
        let bytes = cap.frame_bytes(f);
        row_sum.iter_mut().for_each(|v| *v = 0.0);
        col_sum.iter_mut().for_each(|v| *v = 0.0);
        for (i, &bad) in defect.iter().enumerate() {
            if bad {
                continue;
            }
            let v = f64::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]));
            let (x, y) = (i % w, i / w);
            row_sum[y * 2 + (x & 1)] += v;
            col_sum[x * 2 + (y & 1)] += v;
        }
        let k = (f + 1) as f64;
        for idx in 0..2 * h {
            if row_cnt[idx] == 0 {
                continue;
            }
            let v = row_sum[idx] / row_cnt[idx] as f64;
            let d = v - row_mean[idx];
            row_mean[idx] += d / k;
            row_m2[idx] += d * (v - row_mean[idx]);
        }
        for idx in 0..2 * w {
            if col_cnt[idx] == 0 {
                continue;
            }
            let v = col_sum[idx] / col_cnt[idx] as f64;
            let d = v - col_mean[idx];
            col_mean[idx] += d / k;
            col_m2[idx] += d * (v - col_mean[idx]);
        }
    }

    // Per channel.
    let mut stats = Vec::with_capacity(4);
    for c in Channel::ALL {
        let ci = c.index();
        let good: Vec<usize> = per_channel[ci]
            .iter()
            .copied()
            .filter(|&i| !defect[i])
            .collect();
        let hist_c = &hist[ci * HIST_BINS..(ci + 1) * HIST_BINS];
        let total: u64 = hist_c.iter().sum();
        let sum: f64 = hist_c
            .iter()
            .enumerate()
            .map(|(v, &k)| v as f64 * k as f64)
            .sum();
        let zeros = hist_c[0];
        let mut means: Vec<f64> = good.iter().map(|&i| mean[i]).collect();
        let black_median = median(&mut means);
        let read_noise = means_of(good.iter().map(|&i| sigma[i]));
        let (_, dsnu) = mean_std(good.iter().map(|&i| mean[i]));
        // Row and column fixed pattern: the spread of the channel's row (column) means.
        let rows_c: Vec<f64> = (0..2 * h)
            .filter(|&idx| row_cnt[idx] > 0 && channel_of(idx % 2 + (idx / 2) * w).index() == ci)
            .map(|idx| row_mean[idx])
            .collect();
        let cols_c: Vec<f64> = (0..2 * w)
            .filter(|&idx| col_cnt[idx] > 0 && channel_of((idx / 2) + (idx % 2) * w).index() == ci)
            .map(|idx| col_mean[idx])
            .collect();
        let (_, row_fpn) = mean_std(rows_c.iter().copied());
        let (_, col_fpn) = mean_std(cols_c.iter().copied());
        let row_noise = means_of(
            (0..2 * h)
                .filter(|&idx| {
                    row_cnt[idx] > 0 && channel_of(idx % 2 + (idx / 2) * w).index() == ci
                })
                .map(|idx| (row_m2[idx] / (n as f64 - 1.0)).sqrt()),
        );
        let col_noise = means_of(
            (0..2 * w)
                .filter(|&idx| {
                    col_cnt[idx] > 0 && channel_of((idx / 2) + (idx % 2) * w).index() == ci
                })
                .map(|idx| (col_m2[idx] / (n as f64 - 1.0)).sqrt()),
        );
        stats.push(ChannelStats {
            channel: c,
            black_mean: sum / total as f64,
            black_median,
            read_noise,
            dsnu,
            row_fpn,
            col_fpn,
            row_noise,
            col_noise,
            clipped_zero: zeros as f64 / total as f64,
        });
    }
    let channels: [ChannelStats; 4] = stats
        .try_into()
        .map_err(|_| raw::Error("four channels".into()))?;
    Ok(DarkCapture {
        dir: cap.dir.clone(),
        exposure_us: settings.exposure_us,
        analog_gain: settings.analog_gain,
        steady: settings.steady,
        frames: n,
        bits: cap.bits,
        channels,
        defects,
        temperature: cap.temperature_range(),
        sensor: raw::sensor_name(&cap.meta),
        unit: raw::unit_name(&cap.meta),
    })
}

fn means_of(v: impl Iterator<Item = f64>) -> f64 {
    let (mut s, mut n) = (0.0, 0usize);
    for x in v {
        s += x;
        n += 1;
    }
    if n == 0 { f64::NAN } else { s / n as f64 }
}

/// Opens and analyses each capture directory.
pub fn analyse_dirs(
    dirs: &[PathBuf],
    over: &Overrides,
    opts: &DarkOptions,
) -> raw::Result<Vec<DarkCapture>> {
    dirs.iter()
        .map(|d| analyse(&raw::open(d, over)?, opts))
        .collect()
}

/// One analogue gain's captures, summarised.
#[derive(Clone, Debug, PartialEq)]
pub struct GainSummary {
    /// The analogue gain.
    pub gain: f64,
    /// Captures at this gain (over exposures).
    pub captures: usize,
    /// The black level per channel, median (mean over the captures), raw DN.
    pub black: [f64; 4],
    /// The read noise per channel (mean over the captures), raw DN.
    pub read_noise: [f64; 4],
}

impl GainSummary {
    /// The read noise averaged over the channels.
    pub fn read_noise_all(&self) -> f64 {
        self.read_noise.iter().sum::<f64>() / 4.0
    }
    /// The black level averaged over the channels.
    pub fn black_all(&self) -> f64 {
        self.black.iter().sum::<f64>() / 4.0
    }
}

/// Dark current at one gain: the mean signal's slope against exposure time.
#[derive(Clone, Debug, PartialEq)]
pub struct DarkCurrent {
    /// The analogue gain.
    pub gain: f64,
    /// Exposures used.
    pub exposures: usize,
    /// Slope, raw DN per second (the mean over the channels' means).
    pub slope_per_s: f64,
    /// The mean signal at zero exposure, raw DN.
    pub intercept: f64,
}

/// The sweep over captures: per gain, the fits across gains, and the dark current.
#[derive(Clone, Debug, PartialEq)]
pub struct Sweep {
    /// The captures, by gain then exposure.
    pub captures: Vec<DarkCapture>,
    /// Per distinct gain, in increasing order.
    pub gains: Vec<GainSummary>,
    /// Read noise (channel mean) fitted as `constant + slope × gain`, when there are two gains.
    pub read_noise_fit: Option<(f64, f64)>,
    /// Dark current per gain with at least two exposures.
    pub dark_current: Vec<DarkCurrent>,
    /// Whether any gain's black level moves more than the option's threshold.
    pub black_moves: bool,
}

/// Groups captures by gain and fits the sweep.
pub fn sweep(mut captures: Vec<DarkCapture>, opts: &DarkOptions) -> raw::Result<Sweep> {
    if captures.is_empty() {
        return raw::err("no dark captures");
    }
    captures.sort_by(|a, b| {
        a.analog_gain
            .total_cmp(&b.analog_gain)
            .then(a.exposure_us.total_cmp(&b.exposure_us))
    });
    let mut gains: Vec<GainSummary> = Vec::new();
    let mut i = 0;
    while i < captures.len() {
        let g = captures[i].analog_gain;
        let group: Vec<&DarkCapture> = captures[i..]
            .iter()
            .take_while(|c| (c.analog_gain - g).abs() < 1e-3)
            .collect();
        i += group.len();
        let k = group.len() as f64;
        let mut black = [0.0; 4];
        let mut read_noise = [0.0; 4];
        for c in &group {
            for (j, ch) in c.channels.iter().enumerate() {
                black[j] += ch.black_median / k;
                read_noise[j] += ch.read_noise / k;
            }
        }
        gains.push(GainSummary {
            gain: g,
            captures: group.len(),
            black,
            read_noise,
        });
    }
    let read_noise_fit = (gains.len() >= 2).then(|| {
        let n = gains.len() as f64;
        let (sx, sy) = (
            gains.iter().map(|g| g.gain).sum::<f64>(),
            gains.iter().map(|g| g.read_noise_all()).sum::<f64>(),
        );
        let sxx = gains.iter().map(|g| g.gain * g.gain).sum::<f64>();
        let sxy = gains
            .iter()
            .map(|g| g.gain * g.read_noise_all())
            .sum::<f64>();
        let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
        ((sy - slope * sx) / n, slope)
    });
    let mut dark_current = Vec::new();
    for g in &gains {
        let at: Vec<&DarkCapture> = captures
            .iter()
            .filter(|c| (c.analog_gain - g.gain).abs() < 1e-3)
            .collect();
        if at.len() < 2 {
            continue;
        }
        let pts: Vec<(f64, f64)> = at
            .iter()
            .map(|c| (c.exposure_us * 1e-6, c.mean_black()))
            .collect();
        let n = pts.len() as f64;
        let sx: f64 = pts.iter().map(|p| p.0).sum();
        let sy: f64 = pts.iter().map(|p| p.1).sum();
        let sxx: f64 = pts.iter().map(|p| p.0 * p.0).sum();
        let sxy: f64 = pts.iter().map(|p| p.0 * p.1).sum();
        let den = n * sxx - sx * sx;
        if den.abs() < 1e-18 {
            continue;
        }
        let slope = (n * sxy - sx * sy) / den;
        dark_current.push(DarkCurrent {
            gain: g.gain,
            exposures: at.len(),
            slope_per_s: slope,
            intercept: (sy - slope * sx) / n,
        });
    }
    let black_moves = gains.len() >= 2
        && (0..4).any(|c| {
            let (lo, hi) = gains
                .iter()
                .map(|g| g.black[c])
                .fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(v), h.max(v)));
            hi - lo > opts.black_move
        });
    Ok(Sweep {
        captures,
        gains,
        read_noise_fit,
        dark_current,
        black_moves,
    })
}

/// Opens, analyses and sweeps the capture directories.
pub fn run(dirs: &[PathBuf], over: &Overrides, opts: &DarkOptions) -> raw::Result<Sweep> {
    sweep(analyse_dirs(dirs, over, opts)?, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_spread() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), 2.5);
        let (m, s) = mean_std([1.0, 3.0].into_iter());
        assert_eq!((m, s), (2.0, 1.0));
    }
}
