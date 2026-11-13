//! The ColorChecker in an image: where its 24 patches are ([`Chart`], found automatically by
//! [`detect`] or from four corner patches given by hand), and their values ([`Patch`]).
//!
//! Chart coordinates put patch `i` (0-based, row by row from dark skin) at column `i % 6`, row
//! `i / 6`; a homography maps them to the half-resolution planes (quad coordinates: quad
//! `(x, y)` centred at `(x, y)`).

mod detect;

pub use detect::{Detection, detect};

use crate::linalg::{Mat3, apply, homography};
use crate::raw::Planes;

/// Where the chart is.
#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    /// Chart coordinates (column, row) → plane coordinates.
    pub h: Mat3,
    /// Found automatically, or placed from corners given by hand.
    pub manual: bool,
}

/// One patch's values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Patch {
    /// Centre, plane coordinates.
    pub centre: [f64; 2],
    /// Per-channel trimmed means (R, Gr, Gb, B).
    pub mean: [f64; 4],
    /// Per-channel spatial variance after removing a plane fit (noise within the patch).
    pub variance: [f64; 4],
    /// Per-channel mean temporal variance over the patch, when the capture had several frames.
    pub temporal: Option<[f64; 4]>,
    /// Quads sampled.
    pub count: usize,
}

impl Patch {
    /// `(R, G, B)` with green the mean of the two greens.
    pub fn rgb(&self) -> [f64; 3] {
        [
            self.mean[0],
            (self.mean[1] + self.mean[2]) / 2.0,
            self.mean[3],
        ]
    }
}

impl Chart {
    /// The chart from the centres of the four corner patches in full-resolution pixels: dark
    /// skin (1), bluish green (6), black (24) and white (19), clockwise from top-left as the
    /// chart is printed.
    pub fn from_corners(corners: [[f64; 2]; 4]) -> Option<Self> {
        let src = [[0.0, 0.0], [5.0, 0.0], [5.0, 3.0], [0.0, 3.0]];
        let dst = corners.map(|[x, y]| [x / 2.0 - 0.25, y / 2.0 - 0.25]);
        Some(Self {
            h: homography(&src, &dst)?,
            manual: true,
        })
    }

    /// Centre of patch `i`, plane coordinates.
    pub fn centre(&self, i: usize) -> [f64; 2] {
        apply(&self.h, [(i % 6) as f64, (i / 6) as f64])
    }

    /// Centre of patch `i` in full-resolution pixels.
    pub fn centre_full(&self, i: usize) -> [f64; 2] {
        let [x, y] = self.centre(i);
        [2.0 * x + 0.5, 2.0 * y + 0.5]
    }

    /// Distance between neighbouring patch centres around patch `i`, quads.
    pub fn pitch(&self, i: usize) -> f64 {
        let (c, r) = ((i % 6) as f64, (i / 6) as f64);
        let p = apply(&self.h, [c, r]);
        let dist = |q: [f64; 2]| ((q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2)).sqrt();
        let dx = dist(apply(&self.h, [c + 1.0, r]));
        let dy = dist(apply(&self.h, [c, r + 1.0]));
        dx.min(dy)
    }

    /// The 24 patches' values: a square of `0.4 × pitch` around each centre (inside the patch
    /// even with the chart turned or tilted), `variance` the per-pixel temporal variance of the
    /// capture if any.
    pub fn sample(&self, planes: &Planes, variance: Option<&Planes>) -> [Patch; 24] {
        std::array::from_fn(|i| {
            let [cx, cy] = self.centre(i);
            let half = (0.2 * self.pitch(i)).max(1.0);
            let x0 = (cx - half).ceil().max(0.0) as usize;
            let y0 = (cy - half).ceil().max(0.0) as usize;
            let x1 = ((cx + half).floor() + 1.0).max(0.0) as usize;
            let y1 = ((cy + half).floor() + 1.0).max(0.0) as usize;
            let mean = planes.region(x0, y0, x1, y1).unwrap_or([0.0; 4]);
            let temporal = variance.and_then(|v| v.region(x0, y0, x1, y1));
            let (x1, y1) = (x1.min(planes.width), y1.min(planes.height));
            Patch {
                centre: [cx, cy],
                mean,
                variance: std::array::from_fn(|c| plane_residual(planes, c, x0, y0, x1, y1)),
                temporal,
                count: x1.saturating_sub(x0) * y1.saturating_sub(y0),
            }
        })
    }
}

/// Variance of channel `c` over a region after taking out its best plane `a + bx + cy`.
fn plane_residual(p: &Planes, c: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> f64 {
    let mut rows = Vec::new();
    let mut ys = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            rows.push(vec![1.0, x as f64, y as f64]);
            ys.push(f64::from(p.at(c, x, y)));
        }
    }
    if ys.len() < 6 {
        return 0.0;
    }
    let Some(k) = crate::linalg::lstsq(&rows, &ys, None) else {
        return 0.0;
    };
    let res: Vec<f64> = rows
        .iter()
        .zip(&ys)
        .map(|(r, y)| y - (k[0] + k[1] * r[1] + k[2] * r[2]))
        .collect();
    // Robust: the variance of the residuals within 4 deviations (a defective pixel stays out).
    let mut abs: Vec<f32> = res.iter().map(|v| v.abs() as f32).collect();
    let lim = f64::from(crate::raw::median(&mut abs)) * 1.4826 * 4.0;
    let kept: Vec<f64> = res
        .into_iter()
        .filter(|v| v.abs() <= lim.max(1e-9))
        .collect();
    let n = kept.len() as f64;
    if n < 4.0 {
        return 0.0;
    }
    kept.iter().map(|v| v * v).sum::<f64>() / (n - 3.0)
}
