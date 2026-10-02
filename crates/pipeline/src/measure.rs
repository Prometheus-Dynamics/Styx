//! Measurements of a run: when a series settles, image levels, and image files.

use std::io::Write;
use std::path::Path;

/// The first index from which every value stays within `tolerance` (relative) of the final
/// value, `None` for an empty series.
pub fn settle_index(values: &[f64], tolerance: f64) -> Option<usize> {
    let last = *values.last()?;
    let ok = |v: f64| (v - last).abs() <= tolerance * last.abs().max(1e-12);
    let mut first = values.len() - 1;
    while first > 0 && ok(values[first - 1]) {
        first -= 1;
    }
    Some(first)
}

/// Largest relative excursion from the final value after `from`.
pub fn overshoot(values: &[f64], from: usize) -> f64 {
    let Some(&last) = values.last() else {
        return 0.0;
    };
    values[from.min(values.len())..]
        .iter()
        .map(|v| (v - last).abs() / last.abs().max(1e-12))
        .fold(0.0, f64::max)
}

/// Mean of each channel of a packed RGB24 image.
pub fn rgb_means(rgb: &[u8], width: usize, height: usize, stride: usize) -> [f64; 3] {
    let mut s = [0u64; 3];
    for y in 0..height {
        for px in rgb[y * stride..y * stride + width * 3].chunks_exact(3) {
            for c in 0..3 {
                s[c] += u64::from(px[c]);
            }
        }
    }
    let n = (width * height).max(1) as f64;
    s.map(|v| v as f64 / n)
}

/// Mean of an 8-bit plane.
pub fn plane_mean(data: &[u8], width: usize, height: usize, stride: usize) -> f64 {
    let s: u64 = (0..height)
        .map(|y| {
            data[y * stride..y * stride + width]
                .iter()
                .map(|&v| u64::from(v))
                .sum::<u64>()
        })
        .sum();
    s as f64 / (width * height).max(1) as f64
}

/// How far the image is from grey: `(R/G, B/G)` of the channel means of the pixels whose
/// three channels lie between `lo` and `hi` (neither dark nor clipped). `(1, 1)` is neutral
/// on average (grey world).
pub fn grey_ratios(
    rgb: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    lo: u8,
    hi: u8,
) -> (f64, f64) {
    let mut s = [0u64; 3];
    for y in 0..height {
        for px in rgb[y * stride..y * stride + width * 3].chunks_exact(3) {
            if px.iter().all(|&v| v >= lo && v <= hi) {
                for c in 0..3 {
                    s[c] += u64::from(px[c]);
                }
            }
        }
    }
    let g = s[1].max(1) as f64;
    (s[0] as f64 / g, s[2] as f64 / g)
}

/// Writes a binary PPM (`P6`) of a packed RGB24 image.
pub fn write_ppm(
    path: &Path,
    rgb: &[u8],
    width: usize,
    height: usize,
    stride: usize,
) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(f, "P6\n{width} {height}\n255\n")?;
    for y in 0..height {
        f.write_all(&rgb[y * stride..y * stride + width * 3])?;
    }
    f.flush()
}

/// Writes a binary PGM (`P5`) of an 8-bit plane.
pub fn write_pgm(
    path: &Path,
    data: &[u8],
    width: usize,
    height: usize,
    stride: usize,
) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(f, "P5\n{width} {height}\n255\n")?;
    for y in 0..height {
        f.write_all(&data[y * stride..y * stride + width])?;
    }
    f.flush()
}

/// Converts an NV12 image (BT.601 full range, as the PiSP's "jpeg" encoding writes) to RGB24.
pub fn nv12_to_rgb(y: &[u8], uv: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 3];
    for row in 0..height {
        for col in 0..width {
            let yy = f64::from(y[row * stride + col]);
            let c = (row / 2) * stride + (col / 2) * 2;
            let (u, v) = (f64::from(uv[c]) - 128.0, f64::from(uv[c + 1]) - 128.0);
            let px = [
                yy + 1.402 * v,
                yy - 0.344_136 * u - 0.714_136 * v,
                yy + 1.772 * u,
            ];
            for (k, p) in px.iter().enumerate() {
                out[(row * width + col) * 3 + k] = p.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settling() {
        let v = [0.1, 0.5, 0.9, 1.04, 0.98, 1.0, 1.0];
        assert_eq!(settle_index(&v, 0.05), Some(3));
        assert_eq!(settle_index(&v, 0.01), Some(5));
        assert_eq!(settle_index(&[], 0.1), None);
        assert!((overshoot(&v, 3) - 0.04).abs() < 1e-12);
    }

    #[test]
    fn image_levels() {
        let rgb = [10, 20, 30, 30, 40, 50, 255, 255, 255, 0, 0, 0];
        assert_eq!(rgb_means(&rgb, 2, 1, 12), [20.0, 30.0, 40.0]);
        let (r, b) = grey_ratios(&rgb, 4, 1, 12, 5, 250);
        assert!((r - 40.0 / 60.0).abs() < 1e-12 && (b - 80.0 / 60.0).abs() < 1e-12);
        assert_eq!(plane_mean(&[1, 3, 9, 9], 2, 1, 4), 2.0);
        let grey = nv12_to_rgb(&[100; 4], &[128, 128], 2, 2, 2);
        assert!(grey.iter().all(|&v| v == 100));
    }
}
