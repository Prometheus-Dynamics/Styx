//! Image-quality numbers for raw grey (Y8) captures: frames back to back, rows top to bottom, no
//! padding, no header (the layout of `styx-record` and the libcamera recorder's `.raw` files).
//!
//! Two measurements, each on a region of interest ([`Roi`]):
//!
//! * **Static noise**: over the frames, each pixel's sample standard deviation (Welford, one
//!   pass) on a patch that does not move. The report gives its mean and median over the patch
//!   (`temporal_std`, `temporal_std_median`), and the patch mean. Spatial noise is the robust
//!   sigma of the residual `x - mean(4 neighbours)` in each frame, averaged over frames
//!   (`spatial_sigma`): the RMS of that residual over the patch, with the largest 1% of |residual|
//!   left out (so a hot pixel does not dominate), divided by `sqrt(1.25)`: the residual of white
//!   noise has variance 1.25 sigma squared. Texture inflates it, so use it on a flat patch only.
//!   (A MAD estimate was tried first; 8-bit data makes the median of |residual| a constant 0.25,
//!   so it could not tell captures apart.)
//! * **Edge**: the mean image of the frames over an edge region. Its 10th and 90th percentiles
//!   and their difference (`edge_p10`, `edge_p90`, `edge_contrast`), and the central-difference
//!   gradient energy (`gradient_energy`, mean of gx²+gy²) and mean gradient magnitude
//!   (`gradient_mean`). Higher gradient at the same contrast means a sharper edge; more noise
//!   also raises it, so compare it with the temporal noise.
//!
//! The analysis streams frames, so a 125-frame capture needs one frame and per-pixel accumulators
//! of the ROI at a time.

use std::fmt;

/// A region of interest: `x0..x1` columns and `y0..y1` rows (exclusive ends).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roi {
    /// First column.
    pub x0: usize,
    /// First row.
    pub y0: usize,
    /// One past the last column.
    pub x1: usize,
    /// One past the last row.
    pub y1: usize,
}

impl Roi {
    /// Parses `x0,y0,x1,y1`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.split(',').map(str::trim).collect();
        let [x0, y0, x1, y1] = parts.as_slice() else {
            return Err(format!("region `{text}`: expected x0,y0,x1,y1"));
        };
        let num = |s: &str| {
            s.parse::<usize>()
                .map_err(|_| format!("region `{text}`: `{s}` is not a whole number"))
        };
        let roi = Self {
            x0: num(x0)?,
            y0: num(y0)?,
            x1: num(x1)?,
            y1: num(y1)?,
        };
        if roi.x1 <= roi.x0 || roi.y1 <= roi.y0 {
            return Err(format!("region `{text}`: x1 and y1 must exceed x0 and y0"));
        }
        Ok(roi)
    }

    /// Width in pixels.
    pub fn width(&self) -> usize {
        self.x1 - self.x0
    }

    /// Height in pixels.
    pub fn height(&self) -> usize {
        self.y1 - self.y0
    }

    /// Checks the region lies inside a `w` by `h` frame and is at least 3 pixels on each side
    /// (the gradient and residual need a neighbour each way).
    pub fn check(&self, w: usize, h: usize, what: &str) -> Result<(), String> {
        if self.x1 > w || self.y1 > h {
            return Err(format!("{what} region {self} is outside the {w}x{h} frame"));
        }
        if self.width() < 3 || self.height() < 3 {
            return Err(format!("{what} region {self} must be at least 3x3"));
        }
        Ok(())
    }
}

impl fmt::Display for Roi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{},{},{}", self.x0, self.y0, self.x1, self.y1)
    }
}

/// The number of whole frames in `len` bytes of `w` by `h` grey frames.
pub fn frame_count(len: u64, w: usize, h: usize) -> Result<usize, String> {
    let frame = (w * h) as u64;
    if frame == 0 {
        return Err("frame size is zero".into());
    }
    if !len.is_multiple_of(frame) {
        return Err(format!(
            "{len} bytes is not a whole number of {w}x{h} frames ({frame} bytes each)"
        ));
    }
    Ok((len / frame) as usize)
}

/// The measurements for one capture.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Frames analysed.
    pub frames: usize,
    /// Mean grey level over the noise patch and all frames.
    pub mean: f64,
    /// Mean over the noise patch's pixels of the per-pixel sample standard deviation over frames.
    pub temporal_std: f64,
    /// Median of the same per-pixel deviations.
    pub temporal_std_median: f64,
    /// 90th percentile of the same per-pixel deviations (outlier pixels show up here and in the
    /// mean, not in the median).
    pub temporal_std_p90: f64,
    /// Sample standard deviation over frames of the patch's mean grey level (global brightness
    /// change, such as light flicker, shows up here).
    pub frame_mean_std: f64,
    /// Robust residual sigma of one frame's patch, averaged over frames.
    pub spatial_sigma: f64,
    /// 10th percentile of the edge region's mean image.
    pub edge_p10: f64,
    /// 90th percentile of the edge region's mean image.
    pub edge_p90: f64,
    /// `edge_p90 - edge_p10`.
    pub edge_contrast: f64,
    /// Mean of gx² + gy² over the edge region's mean image.
    pub gradient_energy: f64,
    /// Mean gradient magnitude over the same.
    pub gradient_mean: f64,
}

/// Analyses `frames` (each `w * h` grey bytes, in order). `noise` is the static patch and `edge`
/// the edge region. Errors if the frames do not fit the regions, or fewer than two frames come.
pub fn analyse<I>(frames: I, w: usize, h: usize, noise: Roi, edge: Roi) -> Result<Report, String>
where
    I: IntoIterator<Item = Result<Vec<u8>, String>>,
{
    noise.check(w, h, "noise")?;
    edge.check(w, h, "edge")?;
    let (nw, nh) = (noise.width(), noise.height());
    let n_pix = nw * nh;
    let (ew, eh) = (edge.width(), edge.height());

    // Welford per pixel of the noise patch.
    let mut count = 0usize;
    let mut mean_px = vec![0.0f64; n_pix];
    let mut m2_px = vec![0.0f64; n_pix];
    let mut luma_sum = 0.0f64;
    let mut spatial_sum = 0.0f64;
    // Mean image of the edge region.
    let mut edge_sum = vec![0.0f64; ew * eh];
    let mut patch = vec![0.0f64; n_pix];
    let mut residuals = Vec::with_capacity(nw * nh);
    let mut frame_means = Vec::new();

    for frame in frames {
        let frame = frame?;
        if frame.len() != w * h {
            return Err(format!(
                "frame of {} bytes, expected {}",
                frame.len(),
                w * h
            ));
        }
        count += 1;
        for y in 0..nh {
            let row = (noise.y0 + y) * w + noise.x0;
            for x in 0..nw {
                patch[y * nw + x] = f64::from(frame[row + x]);
            }
        }
        for (i, &v) in patch.iter().enumerate() {
            luma_sum += v;
            let delta = v - mean_px[i];
            mean_px[i] += delta / count as f64;
            m2_px[i] += delta * (v - mean_px[i]);
        }
        frame_means.push(patch.iter().sum::<f64>() / n_pix as f64);
        spatial_sum += residual_sigma(&patch, nw, nh, &mut residuals);
        for y in 0..eh {
            let row = (edge.y0 + y) * w + edge.x0;
            for x in 0..ew {
                edge_sum[y * ew + x] += f64::from(frame[row + x]);
            }
        }
    }
    if count < 2 {
        return Err(format!("need at least 2 frames, got {count}"));
    }

    let devs: Vec<f64> = m2_px
        .iter()
        .map(|m2| (m2 / (count - 1) as f64).sqrt())
        .collect();
    let temporal_std = devs.iter().sum::<f64>() / n_pix as f64;
    let mut sorted_devs = devs.clone();
    sorted_devs.sort_by(f64::total_cmp);
    let temporal_std_median = percentile(&sorted_devs, 0.5);
    let temporal_std_p90 = percentile(&sorted_devs, 0.9);
    let frame_mean_std = sample_std(&frame_means);

    let mut mean_img: Vec<f64> = edge_sum.iter().map(|s| s / count as f64).collect();
    let mut sorted = mean_img.clone();
    sorted.sort_by(f64::total_cmp);
    let edge_p10 = percentile(&sorted, 0.10);
    let edge_p90 = percentile(&sorted, 0.90);
    let (gradient_energy, gradient_mean) = gradients(&mut mean_img, ew, eh);

    Ok(Report {
        frames: count,
        mean: luma_sum / (count * n_pix) as f64,
        temporal_std,
        temporal_std_median,
        temporal_std_p90,
        frame_mean_std,
        spatial_sigma: spatial_sum / count as f64,
        edge_p10,
        edge_p90,
        edge_contrast: edge_p90 - edge_p10,
        gradient_energy,
        gradient_mean,
    })
}

/// Sample standard deviation (n - 1) of `v`; zero for fewer than two values.
fn sample_std(v: &[f64]) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt()
}

/// Sigma of `x - mean(4 neighbours)` over the interior of a `w` by `h` patch: the RMS with the
/// largest 1% of |residual| left out, divided by `sqrt(1.25)` (white noise gives its own sigma).
fn residual_sigma(patch: &[f64], w: usize, h: usize, scratch: &mut Vec<f64>) -> f64 {
    scratch.clear();
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let nb = (patch[i - 1] + patch[i + 1] + patch[i - w] + patch[i + w]) / 4.0;
            scratch.push(patch[i] - nb);
        }
    }
    if scratch.is_empty() {
        return 0.0;
    }
    let mut mags: Vec<f64> = scratch.iter().map(|r| r.abs()).collect();
    let keep = ((mags.len() as f64) * 0.99).ceil() as usize;
    let idx = keep.clamp(1, mags.len()) - 1;
    mags.select_nth_unstable_by(idx, f64::total_cmp);
    let limit = mags[idx];
    let (sum, n) = scratch
        .iter()
        .filter(|r| r.abs() <= limit)
        .fold((0.0, 0usize), |(s, n), r| (s + r * r, n + 1));
    (sum / n.max(1) as f64).sqrt() / 1.25f64.sqrt()
}

/// The value at fraction `p` of a sorted slice (nearest rank).
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

/// Gradient energy and mean magnitude of the interior of a `w` by `h` image, central differences.
/// Zero if the image is smaller than 3x3.
fn gradients(img: &mut [f64], w: usize, h: usize) -> (f64, f64) {
    let mut energy = 0.0;
    let mut mag = 0.0;
    let mut n = 0usize;
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let i = y * w + x;
            let gx = (img[i + 1] - img[i - 1]) / 2.0;
            let gy = (img[i + w] - img[i - w]) / 2.0;
            energy += gx * gx + gy * gy;
            mag += (gx * gx + gy * gy).sqrt();
            n += 1;
        }
    }
    if n == 0 {
        return (0.0, 0.0);
    }
    (energy / n as f64, mag / n as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic standard normal samples (xorshift and Box-Muller).
    struct Gauss {
        state: u64,
    }

    impl Gauss {
        fn next_u(&mut self) -> f64 {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            ((self.state >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }

        fn sample(&mut self) -> f64 {
            let (u1, u2) = (self.next_u(), self.next_u());
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }
    }

    fn frames_with(
        n: usize,
        w: usize,
        h: usize,
        f: impl Fn(usize, usize, usize, &mut Gauss) -> f64,
    ) -> Vec<Result<Vec<u8>, String>> {
        let mut g = Gauss {
            state: 0x9e37_79b9_7f4a_7c15,
        };
        (0..n)
            .map(|_| {
                let mut frame = vec![0u8; w * h];
                for y in 0..h {
                    for x in 0..w {
                        let v = f(x, y, 0, &mut g).round().clamp(0.0, 255.0);
                        frame[y * w + x] = v as u8;
                    }
                }
                Ok(frame)
            })
            .collect()
    }

    #[test]
    fn roi_parses_and_checks() {
        let r = Roi::parse("560,120,760,300").unwrap();
        assert_eq!((r.width(), r.height()), (200, 180));
        assert_eq!(r.to_string(), "560,120,760,300");
        assert!(Roi::parse("1,2,3").is_err());
        assert!(Roi::parse("10,10,5,20").is_err());
        assert!(Roi::parse("a,1,2,3").is_err());
        assert!(r.check(1280, 800, "noise").is_ok());
        assert!(r.check(700, 800, "noise").is_err());
        assert!(
            Roi::parse("0,0,2,9")
                .unwrap()
                .check(10, 10, "edge")
                .is_err()
        );
    }

    #[test]
    fn frame_count_needs_whole_frames() {
        assert_eq!(frame_count(1024 * 800 * 3, 1024, 800).unwrap(), 3);
        assert!(frame_count(1000, 1024, 800).is_err());
        assert!(frame_count(0, 0, 10).is_err());
    }

    #[test]
    fn static_frames_have_no_noise_and_no_gradient() {
        let (w, h) = (64, 48);
        let roi = Roi::parse("8,8,56,40").unwrap();
        let frames = frames_with(10, w, h, |_, _, _, _| 100.0);
        let r = analyse(frames, w, h, roi, roi).unwrap();
        assert_eq!(r.frames, 10);
        assert_eq!(r.mean, 100.0);
        assert_eq!(r.temporal_std, 0.0);
        assert_eq!(r.temporal_std_p90, 0.0);
        assert_eq!(r.frame_mean_std, 0.0);
        assert_eq!(r.spatial_sigma, 0.0);
        assert_eq!(r.edge_contrast, 0.0);
        assert_eq!(r.gradient_energy, 0.0);
    }

    #[test]
    fn white_noise_gives_its_sigma_both_ways() {
        let (w, h) = (64, 48);
        let roi = Roi::parse("4,4,60,44").unwrap();
        let frames = frames_with(60, w, h, |_, _, _, g| 120.0 + 2.0 * g.sample());
        let r = analyse(frames, w, h, roi, roi).unwrap();
        assert!((r.mean - 120.0).abs() < 0.2, "mean {}", r.mean);
        assert!(
            (r.temporal_std - 2.0).abs() < 0.15,
            "temporal {}",
            r.temporal_std
        );
        assert!((r.temporal_std_median - 2.0).abs() < 0.15);
        // The spatial estimate of one frame, averaged over frames.
        assert!(
            (r.spatial_sigma - 2.0).abs() < 0.25,
            "spatial {}",
            r.spatial_sigma
        );
    }

    #[test]
    fn edge_reports_percentiles_and_gradient() {
        let (w, h) = (40, 20);
        let patch = Roi::parse("2,2,38,18").unwrap();
        let edge = Roi::parse("10,2,30,18").unwrap();
        // A step from 50 to 200 at column 20.
        let frames = frames_with(2, w, h, |x, _, _, _| if x < 20 { 50.0 } else { 200.0 });
        let r = analyse(frames, w, h, patch, edge).unwrap();
        assert_eq!(r.edge_p10, 50.0);
        assert_eq!(r.edge_p90, 200.0);
        assert_eq!(r.edge_contrast, 150.0);
        assert!(r.gradient_energy > 0.0);
        assert!(r.gradient_mean > 0.0);
    }

    #[test]
    fn rejects_bad_input() {
        let roi = Roi::parse("0,0,10,10").unwrap();
        let one = frames_with(1, 16, 16, |_, _, _, _| 1.0);
        assert!(
            analyse(one, 16, 16, roi, roi)
                .unwrap_err()
                .contains("at least 2")
        );
        let wrong = vec![Ok(vec![0u8; 10])];
        assert!(analyse(wrong.clone(), 16, 16, roi, roi).is_err());
        let outside = Roi::parse("0,0,20,10").unwrap();
        assert!(analyse(Vec::new(), 16, 16, outside, roi).is_err());
        let err = vec![Err::<Vec<u8>, String>("read failed".into())];
        assert_eq!(analyse(err, 16, 16, roi, roi).unwrap_err(), "read failed");
    }
}
