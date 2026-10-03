//! Colour correction matrices from the ColorChecker, as Raspberry Pi's `ctt` makes them: the
//! patches white balanced on the chart's own greys, a matrix whose rows sum to 1 (greys stay
//! grey) mapping them to the reference colours in linear sRGB, first by least squares in
//! linear RGB, then refined to minimise the colour error in CIELAB.
//!
//! Differences from `ctt`: the refinement minimises the sum of squared ΔE (Levenberg-Marquardt;
//! `ctt` minimises the mean ΔE with a simplex search) and fits the chart's exposure with the
//! matrix instead of fixing it from the mean level; patches with a clipped or unusable channel
//! are left out; a coefficient bound keeps the matrix from amplifying noise without limit
//! (the saturation constraint, `max_coefficient`).

use crate::colour::{WB_GREYS, delta_e76, delta_e2000, linear_srgb_to_lab, macbeth_linear};
use crate::linalg::{IDENTITY, Mat3, levenberg_marquardt, lstsq, mul_vec};

/// A matrix fitted to one chart.
#[derive(Clone, Debug, PartialEq)]
pub struct CcmFit {
    /// Colour temperature of the shot.
    pub ct: f64,
    /// Row-major, rows summing to 1.
    pub ccm: Mat3,
    /// White balance gains `(R, B)` from the chart's greys.
    pub wb: (f64, f64),
    /// Mean ΔE 1976 over the patches used.
    pub mean_de: f64,
    /// Largest ΔE 1976.
    pub max_de: f64,
    /// Mean CIEDE2000.
    pub mean_de2000: f64,
    /// ΔE 1976 per patch (0 for patches left out).
    pub patch_de: [f64; 24],
    /// Patches used.
    pub used: usize,
    /// Mean ΔE with the identity matrix (the white balanced camera colours as they are).
    pub identity_de: f64,
}

/// White balance gains `(R, B)` that make the chart's middle greys neutral.
pub fn grey_gains(rgb: &[[f64; 3]; 24]) -> Option<(f64, f64)> {
    let s = |c: usize| WB_GREYS.map(|i| rgb[i][c]).sum::<f64>();
    let (r, g, b) = (s(0), s(1), s(2));
    (r > 0.0 && g > 0.0 && b > 0.0).then(|| (g / r, g / b))
}

fn de_stats(
    m: &Mat3,
    cam: &[[f64; 3]],
    used: &[bool; 24],
    scale: f64,
) -> ([f64; 24], f64, f64, f64) {
    let reference = macbeth_linear();
    let mut per = [0.0; 24];
    let (mut sum, mut max, mut sum2000, mut n) = (0.0, 0.0f64, 0.0, 0.0);
    for i in (0..24).filter(|&i| used[i]) {
        let out = mul_vec(m, cam[i].map(|v| v * scale));
        let (a, b) = (linear_srgb_to_lab(out), linear_srgb_to_lab(reference[i]));
        per[i] = delta_e76(a, b);
        sum += per[i];
        sum2000 += delta_e2000(a, b);
        max = max.max(per[i]);
        n += 1.0;
    }
    (per, sum / n, max, sum2000 / n)
}

/// Fit a matrix to one chart's patch values (black removed, lens shading corrected; R, G, B).
/// `usable` marks patches with no clipped or near-black channel.
pub fn fit(
    ct: f64,
    rgb: &[[f64; 3]; 24],
    usable: &[bool; 24],
    max_coefficient: f64,
) -> Option<CcmFit> {
    let (wr, wb) = grey_gains(rgb)?;
    let cam: Vec<[f64; 3]> = rgb.iter().map(|p| [p[0] * wr, p[1], p[2] * wb]).collect();
    let reference = macbeth_linear();
    let used: Vec<usize> = (0..24).filter(|&i| usable[i]).collect();
    if used.len() < 12 {
        return None;
    }
    // Exposure: the greys' green against the reference greys.
    let g_cam: f64 = WB_GREYS.map(|i| cam[i][1]).sum();
    let g_ref: f64 = WB_GREYS.map(|i| reference[i][1]).sum();
    let scale0 = g_ref / g_cam.max(1e-12);
    // Linear least squares per row with the row summing to 1.
    let mut m0 = IDENTITY;
    for row in 0..3 {
        let rows: Vec<Vec<f64>> = used
            .iter()
            .map(|&i| {
                let c = cam[i].map(|v| v * scale0);
                vec![c[0] - c[2], c[1] - c[2]]
            })
            .collect();
        let y: Vec<f64> = used
            .iter()
            .map(|&i| reference[i][row] - cam[i][2] * scale0)
            .collect();
        if let Some(k) = lstsq(&rows, &y, None) {
            m0[row * 3] = k[0];
            m0[row * 3 + 1] = k[1];
            m0[row * 3 + 2] = 1.0 - k[0] - k[1];
        }
    }
    let to_m = |p: &[f64]| -> Mat3 {
        [
            p[0],
            p[1],
            1.0 - p[0] - p[1],
            p[2],
            p[3],
            1.0 - p[2] - p[3],
            p[4],
            p[5],
            1.0 - p[4] - p[5],
        ]
    };
    let ref_lab: Vec<[f64; 3]> = reference.iter().map(|r| linear_srgb_to_lab(*r)).collect();
    let residuals = |p: &[f64]| -> Vec<f64> {
        let m = to_m(p);
        let s = p[6].exp();
        let mut out = Vec::with_capacity(used.len() * 3 + 9);
        for &i in &used {
            let lab = linear_srgb_to_lab(mul_vec(&m, cam[i].map(|v| v * s)));
            out.extend((0..3).map(|k| lab[k] - ref_lab[i][k]));
        }
        // The saturation constraint: coefficients beyond the bound cost heavily.
        out.extend(
            m.iter()
                .map(|v| 50.0 * (v.abs() - max_coefficient).max(0.0)),
        );
        out
    };
    let x0 = [m0[0], m0[1], m0[3], m0[4], m0[6], m0[7], scale0.ln()];
    let p = levenberg_marquardt(residuals, &x0, 200);
    let ccm = to_m(&p);
    let scale = p[6].exp();
    let mut mask = [false; 24];
    used.iter().for_each(|&i| mask[i] = true);
    let (patch_de, mean_de, max_de, mean_de2000) = de_stats(&ccm, &cam, &mask, scale);
    // The identity's error at its best exposure (for the report).
    let ref_lab = &ref_lab;
    let ident = levenberg_marquardt(
        |q: &[f64]| {
            used.iter()
                .flat_map(|&i| {
                    let lab = linear_srgb_to_lab(cam[i].map(|v| v * q[0].exp()));
                    (0..3).map(move |k| lab[k] - ref_lab[i][k])
                })
                .collect()
        },
        &[scale0.ln()],
        50,
    );
    let identity_de = de_stats(&IDENTITY, &cam, &mask, ident[0].exp()).1;
    Some(CcmFit {
        ct,
        ccm: ccm.map(|v| (v * 1e5).round() / 1e5),
        wb: (wr, wb),
        mean_de,
        max_de,
        mean_de2000,
        patch_de,
        used: used.len(),
        identity_de,
    })
}

/// Average fits of the same temperature (within 1 K): `(ct, matrix)` in increasing ct.
pub fn combine(fits: &[CcmFit]) -> Vec<(f64, Mat3)> {
    let mut sorted: Vec<&CcmFit> = fits.iter().collect();
    sorted.sort_by(|a, b| a.ct.total_cmp(&b.ct));
    let mut out: Vec<(f64, Mat3, f64)> = Vec::new();
    for f in sorted {
        match out.last_mut() {
            Some((ct, m, n)) if (*ct - f.ct).abs() < 1.0 => {
                for (a, b) in m.iter_mut().zip(&f.ccm) {
                    *a = (*a * *n + b) / (*n + 1.0);
                }
                *n += 1.0;
            }
            _ => out.push((f.ct, f.ccm, 1.0)),
        }
    }
    out.into_iter()
        .map(|(ct, m, _)| (ct, m.map(|v| (v * 1e5).round() / 1e5)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linalg::inverse;

    #[test]
    fn recovers_an_exact_matrix() {
        let truth: Mat3 = [1.8, -0.6, -0.2, -0.4, 1.7, -0.3, 0.05, -0.85, 1.8];
        let inv = inverse(&truth).unwrap();
        let rgb: [[f64; 3]; 24] = std::array::from_fn(|i| {
            let v = mul_vec(&inv, macbeth_linear()[i]);
            [v[0] * 0.6 * 0.3, v[1] * 0.3, v[2] * 0.7 * 0.3]
        });
        let mut usable = [true; 24];
        usable[17] = false; // cyan is clipped in the reference
        let f = fit(5000.0, &rgb, &usable, 4.0).unwrap();
        // The chart's greys are not exactly neutral, so white balancing on them (as ctt does)
        // moves the answer slightly off the matrix the patches were made with.
        for (a, b) in f.ccm.iter().zip(&truth) {
            assert!((a - b).abs() < 0.03, "{:?}", f.ccm);
        }
        assert!(
            f.mean_de < 0.6 && f.identity_de > 3.0,
            "{} {}",
            f.mean_de,
            f.identity_de
        );
        assert!((f.wb.0 * 0.6 - 1.0).abs() < 0.02, "{:?}", f.wb);
    }
}
