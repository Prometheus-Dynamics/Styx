//! Spectra for the synthetic sensor: black-body illuminants, the CIE 1931 colour matching
//! functions (Wyman, Sloan and Shirley's multi-lobe fit), Gaussian channel sensitivities, and
//! ColorChecker-like reflectances made to reproduce the reference colours under a 6504 K
//! black body.

use crate::colour::{SRGB_TO_XYZ, macbeth_linear};
use crate::linalg::{mul_vec, solve};

/// Wavelengths sampled, nm (380..780 in steps of 5).
pub fn wavelengths() -> impl Iterator<Item = f64> {
    (0..81).map(|i| 380.0 + 5.0 * f64::from(i))
}

/// Black-body spectral radiance at `ct` kelvin, relative.
pub fn planck(lambda_nm: f64, ct: f64) -> f64 {
    const C2: f64 = 1.438_776_9e7; // nm·K
    let l = lambda_nm / 560.0;
    1.0 / (l.powi(5) * ((C2 / (lambda_nm * ct)).exp() - 1.0))
}

fn lobe(l: f64, mu: f64, s1: f64, s2: f64) -> f64 {
    let s = if l < mu { s1 } else { s2 };
    (-0.5 * ((l - mu) / s).powi(2)).exp()
}

/// CIE 1931 2° colour matching functions `(x̄, ȳ, z̄)`.
pub fn cmf(l: f64) -> [f64; 3] {
    [
        1.056 * lobe(l, 599.8, 37.9, 31.0) + 0.362 * lobe(l, 442.0, 16.0, 26.7)
            - 0.065 * lobe(l, 501.1, 20.4, 26.2),
        0.821 * lobe(l, 568.8, 46.9, 40.5) + 0.286 * lobe(l, 530.9, 16.3, 31.1),
        1.217 * lobe(l, 437.0, 11.8, 36.0) + 0.681 * lobe(l, 459.0, 26.0, 13.8),
    ]
}

/// A sensor's channel sensitivities: Gaussians `(peak, width)` for R, G, B, plus a fraction of
/// green in red and blue (real filters overlap).
#[derive(Clone, Debug, PartialEq)]
pub struct Sensitivities {
    /// `(peak nm, standard deviation nm)` for R, G, B.
    pub lobes: [(f64, f64); 3],
}

impl Default for Sensitivities {
    fn default() -> Self {
        Self {
            lobes: [(605.0, 38.0), (535.0, 42.0), (455.0, 30.0)],
        }
    }
}

impl Sensitivities {
    /// Channel responses at a wavelength.
    pub fn at(&self, l: f64) -> [f64; 3] {
        self.lobes
            .map(|(mu, s)| (-0.5 * ((l - mu) / s).powi(2)).exp())
    }
}

/// Smooth reflectance basis: a constant and Gaussian bumps across the visible range.
fn basis(l: f64) -> [f64; 6] {
    let g = |mu: f64| (-0.5 * ((l - mu) / 45.0).powi(2)).exp();
    [1.0, g(420.0), g(480.0), g(540.0), g(600.0), g(660.0)]
}

/// A reflectance spectrum, sampled on [`wavelengths`].
pub type Spectrum = Vec<f64>;

/// The 24 patches as spectra whose colours under a 6504 K black body (white normalised to
/// Y = 1) are the reference linear sRGB values (as closely as reflectances in [0.01, 1] allow).
pub fn macbeth_spectra() -> Vec<Spectrum> {
    let ls: Vec<f64> = wavelengths().collect();
    let illum: Vec<f64> = ls.iter().map(|&l| planck(l, 6504.0)).collect();
    let norm: f64 = ls.iter().zip(&illum).map(|(&l, e)| e * cmf(l)[1]).sum();
    // XYZ of each basis function under the illuminant.
    let mut bx = [[0.0; 6]; 3];
    for (&l, e) in ls.iter().zip(&illum) {
        let c = cmf(l);
        let b = basis(l);
        for k in 0..3 {
            for j in 0..6 {
                bx[k][j] += e * c[k] * b[j] / norm;
            }
        }
    }
    // The illuminant's white maps to XYZ of linear sRGB white scaled to this black body.
    let white_xyz: [f64; 3] = std::array::from_fn(|k| bx[k][0]);
    let d65 = mul_vec(&SRGB_TO_XYZ, [1.0, 1.0, 1.0]);
    macbeth_linear()
        .iter()
        .map(|rgb| {
            let xyz = mul_vec(&SRGB_TO_XYZ, *rgb);
            // Von Kries in XYZ from D65 to the black body's white (they are close).
            let target: [f64; 3] = std::array::from_fn(|k| xyz[k] * white_xyz[k] / d65[k]);
            // Weights nearest a flat spectrum of the same luminance that hit the target:
            // minimise |w - w0|² subject to bx w = target.
            let mut w0 = [0.0; 6];
            w0[0] = target[1];
            let resid: [f64; 3] =
                std::array::from_fn(|k| target[k] - (0..6).map(|j| bx[k][j] * w0[j]).sum::<f64>());
            let gram: Vec<f64> = (0..9)
                .map(|i| (0..6).map(|j| bx[i / 3][j] * bx[i % 3][j]).sum())
                .collect();
            let lam = solve(gram, resid.to_vec()).unwrap_or(vec![0.0; 3]);
            let w: Vec<f64> = (0..6)
                .map(|j| w0[j] + (0..3).map(|k| bx[k][j] * lam[k]).sum::<f64>())
                .collect();
            ls.iter()
                .map(|&l| {
                    let b = basis(l);
                    (0..6).map(|j| w[j] * b[j]).sum::<f64>().clamp(0.01, 1.0)
                })
                .collect()
        })
        .collect()
}

/// Camera RGB of a reflectance under a black body at `ct`, relative to a perfect white's green
/// under the same light.
pub fn camera_rgb(s: &Sensitivities, reflectance: &[f64], ct: f64) -> [f64; 3] {
    let mut out = [0.0; 3];
    let mut white_g = 0.0;
    for (i, l) in wavelengths().enumerate() {
        let e = planck(l, ct);
        let c = s.at(l);
        for k in 0..3 {
            out[k] += e * c[k] * reflectance[i];
        }
        white_g += e * c[1];
    }
    out.map(|v| v / white_g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colour::{delta_e2000, linear_srgb_to_lab, xyz_to_lab};

    #[test]
    fn spectra_reproduce_the_chart() {
        let ls: Vec<f64> = wavelengths().collect();
        let illum: Vec<f64> = ls.iter().map(|&l| planck(l, 6504.0)).collect();
        let norm: f64 = ls.iter().zip(&illum).map(|(&l, e)| e * cmf(l)[1]).sum();
        let white: [f64; 3] = std::array::from_fn(|k| {
            ls.iter()
                .zip(&illum)
                .map(|(&l, e)| e * cmf(l)[k])
                .sum::<f64>()
                / norm
        });
        let mut worst: f64 = 0.0;
        for (spec, rgb) in macbeth_spectra().iter().zip(macbeth_linear()) {
            let xyz: [f64; 3] = std::array::from_fn(|k| {
                ls.iter()
                    .zip(&illum)
                    .zip(spec)
                    .map(|((&l, e), r)| e * cmf(l)[k] * r)
                    .sum::<f64>()
                    / norm
            });
            worst = worst.max(delta_e2000(xyz_to_lab(xyz, white), linear_srgb_to_lab(rgb)));
        }
        // Cyan is outside what smooth reflectances reach; the rest are close.
        assert!(worst < 8.0, "worst ΔE {worst}");
    }
}
