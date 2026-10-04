//! Small dense linear algebra: 3×3 matrices, least squares, homographies, a
//! Levenberg-Marquardt solver, polynomial fits.

/// Row-major 3×3 matrix.
pub type Mat3 = [f64; 9];

/// The identity.
pub const IDENTITY: Mat3 = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// `a × b`.
pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    std::array::from_fn(|k| {
        let (i, j) = (k / 3, k % 3);
        (0..3).map(|m| a[i * 3 + m] * b[m * 3 + j]).sum()
    })
}

/// `m × v`.
pub fn mul_vec(m: &Mat3, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| (0..3).map(|j| m[i * 3 + j] * v[j]).sum())
}

/// The inverse, if `m` is not singular.
pub fn inverse(m: &Mat3) -> Option<Mat3> {
    let c = |a: usize, b: usize, c: usize, d: usize| m[a] * m[d] - m[b] * m[c];
    let cof = [
        c(4, 5, 7, 8),
        -c(3, 5, 6, 8),
        c(3, 4, 6, 7),
        -c(1, 2, 7, 8),
        c(0, 2, 6, 8),
        -c(0, 1, 6, 7),
        c(1, 2, 4, 5),
        -c(0, 2, 3, 5),
        c(0, 1, 3, 4),
    ];
    let det = m[0] * cof[0] + m[1] * cof[1] + m[2] * cof[2];
    if det.abs() < 1e-300 {
        return None;
    }
    // Inverse = adjugate / det; the adjugate is the transposed cofactor matrix.
    Some(std::array::from_fn(|k| cof[(k % 3) * 3 + k / 3] / det))
}

/// Solve `a x = b` (`a` row-major `n × n`) by Gaussian elimination with partial pivoting.
pub fn solve(mut a: Vec<f64>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let p = (col..n).max_by(|&i, &j| a[i * n + col].abs().total_cmp(&a[j * n + col].abs()))?;
        if a[p * n + col].abs() < 1e-14 {
            return None;
        }
        if p != col {
            for k in 0..n {
                a.swap(p * n + k, col * n + k);
            }
            b.swap(p, col);
        }
        for r in col + 1..n {
            let f = a[r * n + col] / a[col * n + col];
            if f != 0.0 {
                for k in col..n {
                    a[r * n + k] -= f * a[col * n + k];
                }
                b[r] -= f * b[col];
            }
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let s: f64 = (r + 1..n).map(|k| a[r * n + k] * x[k]).sum();
        x[r] = (b[r] - s) / a[r * n + r];
    }
    Some(x)
}

/// Weighted least squares: `x` minimising `Σ w_i (rows_i · x - y_i)²`.
pub fn lstsq(rows: &[Vec<f64>], y: &[f64], w: Option<&[f64]>) -> Option<Vec<f64>> {
    let n = rows.first()?.len();
    let mut ata = vec![0.0; n * n];
    let mut aty = vec![0.0; n];
    for (i, (r, &yi)) in rows.iter().zip(y).enumerate() {
        let wi = w.map_or(1.0, |w| w[i]);
        for a in 0..n {
            aty[a] += wi * r[a] * yi;
            for b in 0..n {
                ata[a * n + b] += wi * r[a] * r[b];
            }
        }
    }
    solve(ata, aty)
}

/// Polynomial of `degree` through `(x, y)` by least squares; coefficients from the constant up.
pub fn polyfit(x: &[f64], y: &[f64], degree: usize) -> Option<Vec<f64>> {
    let rows: Vec<Vec<f64>> = x
        .iter()
        .map(|&v| (0..=degree).map(|p| v.powi(p as i32)).collect())
        .collect();
    lstsq(&rows, y, None)
}

/// Evaluate a polynomial (coefficients from the constant up).
pub fn polyval(c: &[f64], x: f64) -> f64 {
    c.iter().rev().fold(0.0, |acc, k| acc * x + k)
}

/// A homography mapping `src` points to `dst` (at least 4 pairs), least squares with
/// `h[8] = 1`, in coordinates normalised for conditioning.
pub fn homography(src: &[[f64; 2]], dst: &[[f64; 2]]) -> Option<Mat3> {
    if src.len() < 4 || src.len() != dst.len() {
        return None;
    }
    let norm = |p: &[[f64; 2]]| -> Mat3 {
        let n = p.len() as f64;
        let (mx, my) = (
            p.iter().map(|v| v[0]).sum::<f64>() / n,
            p.iter().map(|v| v[1]).sum::<f64>() / n,
        );
        let d = p
            .iter()
            .map(|v| ((v[0] - mx).powi(2) + (v[1] - my).powi(2)).sqrt())
            .sum::<f64>()
            / n;
        let s = if d > 0.0 {
            std::f64::consts::SQRT_2 / d
        } else {
            1.0
        };
        [s, 0.0, -s * mx, 0.0, s, -s * my, 0.0, 0.0, 1.0]
    };
    let (ts, td) = (norm(src), norm(dst));
    let mut rows = Vec::new();
    let mut y = Vec::new();
    for (s, d) in src.iter().zip(dst) {
        let [x, yy] = apply(&ts, *s);
        let [u, v] = apply(&td, *d);
        rows.push(vec![x, yy, 1.0, 0.0, 0.0, 0.0, -u * x, -u * yy]);
        y.push(u);
        rows.push(vec![0.0, 0.0, 0.0, x, yy, 1.0, -v * x, -v * yy]);
        y.push(v);
    }
    let h = lstsq(&rows, &y, None)?;
    let hn: Mat3 = [h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], 1.0];
    let m = mul(&inverse(&td)?, &mul(&hn, &ts));
    (m[8].abs() > 1e-300).then(|| m.map(|v| v / m[8]))
}

/// A point through a homography.
pub fn apply(h: &Mat3, p: [f64; 2]) -> [f64; 2] {
    let [x, y, w] = mul_vec(h, [p[0], p[1], 1.0]);
    [x / w, y / w]
}

/// Levenberg-Marquardt on `residuals(x)` from `x0` with a forward-difference Jacobian; returns
/// the parameters that minimise the sum of squares.
pub fn levenberg_marquardt(
    residuals: impl Fn(&[f64]) -> Vec<f64>,
    x0: &[f64],
    iterations: usize,
) -> Vec<f64> {
    let n = x0.len();
    let mut x = x0.to_vec();
    let mut r = residuals(&x);
    let mut cost: f64 = r.iter().map(|v| v * v).sum();
    let mut lambda = 1e-3;
    for _ in 0..iterations {
        let jac: Vec<Vec<f64>> = (0..n)
            .map(|j| {
                let h = 1e-6 * x[j].abs().max(1e-3);
                let mut xp = x.clone();
                xp[j] += h;
                residuals(&xp)
                    .iter()
                    .zip(&r)
                    .map(|(a, b)| (a - b) / h)
                    .collect()
            })
            .collect();
        let mut jtj = vec![0.0; n * n];
        let mut jtr = vec![0.0; n];
        for a in 0..n {
            for (k, rk) in r.iter().enumerate() {
                jtr[a] -= jac[a][k] * rk;
            }
            for b in 0..n {
                jtj[a * n + b] = jac[a].iter().zip(&jac[b]).map(|(p, q)| p * q).sum();
            }
        }
        let mut improved = false;
        for _ in 0..10 {
            let mut m = jtj.clone();
            for d in 0..n {
                m[d * n + d] *= 1.0 + lambda;
                m[d * n + d] += 1e-12;
            }
            let Some(step) = solve(m, jtr.clone()) else {
                lambda *= 10.0;
                continue;
            };
            let xn: Vec<f64> = x.iter().zip(&step).map(|(a, s)| a + s).collect();
            let rn = residuals(&xn);
            let cn: f64 = rn.iter().map(|v| v * v).sum();
            if cn < cost {
                let done = cost - cn < 1e-12 * cost.max(1e-30);
                (x, r, cost) = (xn, rn, cn);
                lambda = (lambda / 3.0).max(1e-12);
                improved = !done;
                break;
            }
            lambda *= 10.0;
        }
        if !improved {
            break;
        }
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_and_solve() {
        let m = [2.0, 1.0, 0.0, 0.5, 3.0, 1.0, 0.0, 1.0, 4.0];
        let i = inverse(&m).unwrap();
        let p = mul(&m, &i);
        for (k, v) in p.iter().enumerate() {
            assert!((v - IDENTITY[k]).abs() < 1e-12);
        }
        let x = solve(m.to_vec(), vec![1.0, 2.0, 3.0]).unwrap();
        let back = mul_vec(&m, [x[0], x[1], x[2]]);
        assert!((back[2] - 3.0).abs() < 1e-12);
    }

    #[test]
    fn homography_recovers_a_projection() {
        let h: Mat3 = [1.2, 0.1, 30.0, -0.05, 0.9, 12.0, 1e-4, -2e-4, 1.0];
        let src: Vec<[f64; 2]> = (0..12)
            .map(|i| [(i % 4) as f64 * 50.0, (i / 4) as f64 * 40.0])
            .collect();
        let dst: Vec<[f64; 2]> = src.iter().map(|p| apply(&h, *p)).collect();
        let g = homography(&src, &dst).unwrap();
        for (a, b) in g.iter().zip(&h) {
            assert!((a - b).abs() < 1e-8, "{g:?}");
        }
    }

    #[test]
    fn lm_fits_an_exponential() {
        let xs: Vec<f64> = (0..20).map(|i| i as f64 * 0.1).collect();
        let f = |p: &[f64]| {
            xs.iter()
                .map(|x| p[0] * (p[1] * x).exp() - 2.0 * (0.7 * x).exp())
                .collect()
        };
        let p = levenberg_marquardt(f, &[1.0, 0.1], 100);
        assert!(
            (p[0] - 2.0).abs() < 1e-6 && (p[1] - 0.7).abs() < 1e-6,
            "{p:?}"
        );
        let c = polyfit(&[0.0, 1.0, 2.0, 3.0], &[1.0, 3.0, 7.0, 13.0], 2).unwrap();
        assert!((polyval(&c, 4.0) - 21.0).abs() < 1e-9);
    }
}
