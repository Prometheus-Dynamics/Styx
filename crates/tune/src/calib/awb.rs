//! The AWB colour temperature curve from greys at known temperatures, the method of Raspberry
//! Pi's `ctt`: each shot's R/G and B/G of its grey patches (lens shading colour tables
//! applied); a quadratic fitted in "hat space" (`r̂ = r / (1 + r + b)`, `b̂ = b / (1 + r + b)`,
//! where the curve is better behaved); each point's distance from the fit sets how far the
//! search may leave the curve (`transverse_pos/neg`, +10%, at least 0.01); the curve's points
//! are the fit at each shot, keeping temperature decreasing as R/G increases (of two points
//! out of order, the one further from the fit goes).

/// One measured grey.
#[derive(Clone, Debug, PartialEq)]
pub struct AwbPoint {
    /// Colour temperature.
    pub ct: f64,
    /// R/G.
    pub r: f64,
    /// B/G.
    pub b: f64,
    /// Where it came from.
    pub source: String,
}

/// The AWB calibration.
#[derive(Clone, Debug, PartialEq)]
pub struct AwbResult {
    /// The measured points (same temperatures averaged), increasing temperature.
    pub points: Vec<AwbPoint>,
    /// `[ct, r, b]` in increasing temperature.
    pub curve: Vec<[f64; 3]>,
    /// Search margin on the positive side.
    pub transverse_pos: f64,
    /// On the negative side.
    pub transverse_neg: f64,
    /// Temperatures dropped to keep the curve monotonic.
    pub dropped: Vec<f64>,
}

fn hat(r: f64, b: f64) -> (f64, f64) {
    (r / (1.0 + r + b), b / (1.0 + r + b))
}

fn dehat(rh: f64, bh: f64) -> (f64, f64) {
    (rh / (1.0 - rh - bh), bh / (1.0 - rh - bh))
}

/// Fit the curve; `None` with fewer than two temperatures.
pub fn calibrate(points: &[AwbPoint]) -> Option<AwbResult> {
    // Average points of the same temperature.
    let mut pts: Vec<AwbPoint> = Vec::new();
    let mut sorted = points.to_vec();
    sorted.sort_by(|a, b| a.ct.total_cmp(&b.ct));
    let mut counts = Vec::new();
    for p in sorted {
        match pts.last_mut() {
            Some(last) if (last.ct - p.ct).abs() < 1.0 => {
                let n: &mut f64 = counts.last_mut()?;
                last.r = (last.r * *n + p.r) / (*n + 1.0);
                last.b = (last.b * *n + p.b) / (*n + 1.0);
                last.source = format!("{}, {}", last.source, p.source);
                *n += 1.0;
            }
            _ => {
                pts.push(p);
                counts.push(1.0);
            }
        }
    }
    if pts.len() < 2 {
        return None;
    }
    let hats: Vec<(f64, f64)> = pts.iter().map(|p| hat(p.r, p.b)).collect();
    let xs: Vec<f64> = hats.iter().map(|h| h.0).collect();
    let ys: Vec<f64> = hats.iter().map(|h| h.1).collect();
    let degree = if pts.len() >= 3 { 2 } else { 1 };
    let coef = crate::linalg::polyfit(&xs, &ys, degree)?;
    let f = |x: f64| crate::linalg::polyval(&coef, x);
    // Signed distance of each point from the fit, in r/b space: positive above the curve.
    let span = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        - xs.iter().copied().fold(f64::INFINITY, f64::min);
    let dists: Vec<f64> = hats
        .iter()
        .map(|&(x0, y0)| {
            // Closest point on the fit in hat space: dense search then refinement.
            let d2 = |x: f64| (x - x0).powi(2) + (f(x) - y0).powi(2);
            let (mut lo, mut hi) = (x0 - span.max(0.01), x0 + span.max(0.01));
            for _ in 0..100 {
                let (a, b) = (lo + (hi - lo) / 3.0, hi - (hi - lo) / 3.0);
                if d2(a) < d2(b) {
                    hi = b;
                } else {
                    lo = a;
                }
            }
            let xc = (lo + hi) / 2.0;
            let (rc, bc) = dehat(xc, f(xc));
            let (rp, bp) = dehat(x0, y0);
            let d = ((rc - rp).powi(2) + (bc - bp).powi(2)).sqrt();
            if rc + bc > rp + bp { -d } else { d }
        })
        .collect();
    let transverse_neg = (-dists.iter().copied().fold(0.0, f64::min) * 1.1).max(0.01);
    let transverse_pos = (dists.iter().copied().fold(0.0, f64::max) * 1.1).max(0.01);
    // Fitted points, in increasing r̂; temperature must fall along them.
    let mut fitted: Vec<(f64, f64, f64, f64)> = pts
        .iter()
        .zip(&hats)
        .zip(&dists)
        .map(|((p, &(x, _)), d)| {
            let (r, b) = dehat(x, f(x));
            (p.ct, r, b, d.abs())
        })
        .collect();
    fitted.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut dropped = Vec::new();
    let mut i = fitted.len() - 1;
    while i > 0 {
        if fitted[i].0 > fitted[i - 1].0 {
            let bad = if fitted[i - 1].3 > fitted[i].3 {
                i - 1
            } else {
                i
            };
            dropped.push(fitted[bad].0);
            fitted.remove(bad);
        }
        i = i.saturating_sub(1).min(fitted.len().saturating_sub(1));
    }
    let round = |v: f64| (v * 1e4).round() / 1e4;
    let mut curve: Vec<[f64; 3]> = fitted
        .iter()
        .map(|p| [p.0, round(p.1), round(p.2)])
        .collect();
    curve.sort_by(|a, b| a[0].total_cmp(&b[0]));
    Some(AwbResult {
        points: pts,
        curve,
        transverse_pos: (transverse_pos * 1e5).round() / 1e5,
        transverse_neg: (transverse_neg * 1e5).round() / 1e5,
        dropped,
    })
}

/// Default AWB priors and modes when the base tuning has none: the shape Raspberry Pi tunings
/// use (low light favours warm light, daylight levels favour 4000-7000 K), modes clipped to
/// the calibrated range.
pub fn default_priors() -> Vec<(f64, Vec<f64>)> {
    vec![
        (0.0, vec![2000.0, 1.0, 3000.0, 0.0, 13000.0, 0.0]),
        (800.0, vec![2000.0, 0.0, 6000.0, 2.0, 13000.0, 2.0]),
        (
            1500.0,
            vec![
                2000.0, 0.0, 4000.0, 1.0, 6000.0, 6.0, 6500.0, 7.0, 7000.0, 1.0, 13000.0, 1.0,
            ],
        ),
    ]
}

/// Default modes `(name, lo, hi)`, the first the default.
pub fn default_modes(lo: f64, hi: f64) -> Vec<(&'static str, f64, f64)> {
    let modes = [
        ("auto", 2500.0, 8000.0),
        ("incandescent", 2500.0, 3000.0),
        ("tungsten", 3000.0, 3500.0),
        ("fluorescent", 4000.0, 4700.0),
        ("indoor", 3000.0, 5000.0),
        ("daylight", 5500.0, 6500.0),
        ("cloudy", 7000.0, 8600.0),
    ];
    modes
        .iter()
        .map(|&(n, a, b): &(&str, f64, f64)| (n, a.max(lo).min(hi), b.min(hi).max(lo)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_and_keeps_the_curve_monotonic() {
        let p = |ct: f64, r: f64, b: f64| AwbPoint {
            ct,
            r,
            b,
            source: String::new(),
        };
        let pts = [
            p(2800.0, 0.95, 0.42),
            p(4000.0, 0.72, 0.58),
            p(5000.0, 0.62, 0.68),
            p(6500.0, 0.52, 0.78),
        ];
        let a = calibrate(&pts).unwrap();
        assert_eq!(a.curve.len(), 4);
        for (c, q) in a.curve.iter().zip(&pts) {
            assert_eq!(c[0], q.ct);
            assert!(
                (c[1] - q.r).abs() < 0.01 && (c[2] - q.b).abs() < 0.01,
                "{c:?}"
            );
        }
        assert!(a.transverse_pos >= 0.01 && a.transverse_neg >= 0.01);
        // A point out of order is dropped.
        let mut bad = pts.to_vec();
        bad.push(p(7500.0, 0.66, 0.66));
        let a = calibrate(&bad).unwrap();
        assert!(!a.dropped.is_empty());
        assert!(a.curve.windows(2).all(|w| w[1][1] < w[0][1]));
    }
}
