//! Rotations and the lens of a simulated camera: OpenCV's camera models (the equidistant
//! fisheye, and the pinhole with rational radial and tangential distortion).

/// A 3x3 matrix, rows.
pub(crate) type Mat3 = [[f64; 3]; 3];
pub(crate) type Vec3 = [f64; 3];

pub(crate) fn mul(a: &Mat3, v: Vec3) -> Vec3 {
    [0, 1, 2].map(|i| a[i][0] * v[0] + a[i][1] * v[1] + a[i][2] * v[2])
}

pub(crate) fn mul_t(a: &Mat3, v: Vec3) -> Vec3 {
    [0, 1, 2].map(|i| a[0][i] * v[0] + a[1][i] * v[1] + a[2][i] * v[2])
}

pub(crate) fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// The rotation of a unit quaternion (w, x, y, z).
pub(crate) fn from_quaternion([w, x, y, z]: [f64; 4]) -> Mat3 {
    let n = (w * w + x * x + y * y + z * z).sqrt().max(1e-12);
    let (w, x, y, z) = (w / n, x / n, y / n, z / n);
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - w * z),
            2.0 * (x * z + w * y),
        ],
        [
            2.0 * (x * y + w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - w * x),
        ],
        [
            2.0 * (x * z - w * y),
            2.0 * (y * z + w * x),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}

/// The unit quaternion (w, x, y, z) of a rotation.
pub(crate) fn to_quaternion(m: &Mat3) -> [f64; 4] {
    let t = m[0][0] + m[1][1] + m[2][2];
    if t > 0.0 {
        let s = (t + 1.0).sqrt() * 2.0;
        [
            0.25 * s,
            (m[2][1] - m[1][2]) / s,
            (m[0][2] - m[2][0]) / s,
            (m[1][0] - m[0][1]) / s,
        ]
    } else if m[0][0] > m[1][1] && m[0][0] > m[2][2] {
        let s = (1.0 + m[0][0] - m[1][1] - m[2][2]).sqrt() * 2.0;
        [
            (m[2][1] - m[1][2]) / s,
            0.25 * s,
            (m[0][1] + m[1][0]) / s,
            (m[0][2] + m[2][0]) / s,
        ]
    } else if m[1][1] > m[2][2] {
        let s = (1.0 + m[1][1] - m[0][0] - m[2][2]).sqrt() * 2.0;
        [
            (m[0][2] - m[2][0]) / s,
            (m[0][1] + m[1][0]) / s,
            0.25 * s,
            (m[1][2] + m[2][1]) / s,
        ]
    } else {
        let s = (1.0 + m[2][2] - m[0][0] - m[1][1]).sqrt() * 2.0;
        [
            (m[1][0] - m[0][1]) / s,
            (m[0][2] + m[2][0]) / s,
            (m[1][2] + m[2][1]) / s,
            0.25 * s,
        ]
    }
}

/// A lens model and its coefficients.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Model {
    /// OpenCV's fisheye (equidistant): `θd = θ (1 + k1 θ² + k2 θ⁴ + k3 θ⁶ + k4 θ⁸)`.
    Fisheye([f64; 4]),
    /// OpenCV's pinhole: radial `k1..k6` (rational) and tangential `p1, p2`.
    Pinhole { k: [f64; 6], p: [f64; 2] },
}

/// A calibrated camera: the image size the calibration is for, the intrinsics and the lens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Lens {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fx: f64,
    pub(crate) fy: f64,
    pub(crate) cx: f64,
    pub(crate) cy: f64,
    pub(crate) model: Model,
}

impl Lens {
    /// The pixel a point in the optical frame images to, if it can (in front of the lens, or
    /// within a fisheye's field).
    pub(crate) fn project(&self, p: Vec3) -> Option<[f64; 2]> {
        let (x, y) = match self.model {
            Model::Fisheye(k) => {
                let r = (p[0] * p[0] + p[1] * p[1]).sqrt();
                let theta = r.atan2(p[2]);
                if theta > 1.6 {
                    return None;
                }
                let t2 = theta * theta;
                let td = theta * (1.0 + t2 * (k[0] + t2 * (k[1] + t2 * (k[2] + t2 * k[3]))));
                if r < 1e-12 {
                    (0.0, 0.0)
                } else {
                    (td * p[0] / r, td * p[1] / r)
                }
            }
            Model::Pinhole { k, p: t } => {
                if p[2] <= 1e-9 {
                    return None;
                }
                let (a, b) = (p[0] / p[2], p[1] / p[2]);
                let r2 = a * a + b * b;
                let radial = (1.0 + r2 * (k[0] + r2 * (k[1] + r2 * k[2])))
                    / (1.0 + r2 * (k[3] + r2 * (k[4] + r2 * k[5])));
                (
                    a * radial + 2.0 * t[0] * a * b + t[1] * (r2 + 2.0 * a * a),
                    b * radial + t[0] * (r2 + 2.0 * b * b) + 2.0 * t[1] * a * b,
                )
            }
        };
        Some([self.fx * x + self.cx, self.fy * y + self.cy])
    }

    /// The ray (optical frame, unit length) through pixel `(u, v)`.
    pub(crate) fn unproject(&self, u: f64, v: f64) -> Vec3 {
        let (xd, yd) = ((u - self.cx) / self.fx, (v - self.cy) / self.fy);
        match self.model {
            Model::Fisheye(k) => {
                let td = (xd * xd + yd * yd).sqrt();
                if td < 1e-12 {
                    return [0.0, 0.0, 1.0];
                }
                // Newton on θ (1 + k1 θ² + ...) = θd.
                let mut theta = td;
                for _ in 0..10 {
                    let t2 = theta * theta;
                    let f =
                        theta * (1.0 + t2 * (k[0] + t2 * (k[1] + t2 * (k[2] + t2 * k[3])))) - td;
                    let df = 1.0
                        + t2 * (3.0 * k[0]
                            + t2 * (5.0 * k[1] + t2 * (7.0 * k[2] + 9.0 * t2 * k[3])));
                    theta -= f / df.max(1e-6);
                }
                let s = theta.sin() / td;
                [xd * s, yd * s, theta.cos()]
            }
            Model::Pinhole { k, p: t } => {
                // OpenCV's fixed-point undistortion.
                let (mut a, mut b) = (xd, yd);
                for _ in 0..12 {
                    let r2 = a * a + b * b;
                    let radial = (1.0 + r2 * (k[0] + r2 * (k[1] + r2 * k[2])))
                        / (1.0 + r2 * (k[3] + r2 * (k[4] + r2 * k[5])));
                    let da = 2.0 * t[0] * a * b + t[1] * (r2 + 2.0 * a * a);
                    let db = t[0] * (r2 + 2.0 * b * b) + 2.0 * t[1] * a * b;
                    a = (xd - da) / radial;
                    b = (yd - db) / radial;
                }
                let n = (a * a + b * b + 1.0).sqrt();
                [a / n, b / n, 1.0 / n]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn lenses_project_and_unproject_alike() {
        for model in [
            Model::Fisheye([0.1, -0.02, 0.004, -0.0005]),
            Model::Pinhole {
                k: [-0.2, 0.05, 0.0, 0.0, 0.0, 0.0],
                p: [0.001, -0.002],
            },
        ] {
            let lens = Lens {
                width: 1280,
                height: 800,
                fx: 560.0,
                fy: 562.0,
                cx: 639.5,
                cy: 399.5,
                model,
            };
            for (u, v) in [
                (639.5, 399.5),
                (100.0, 50.0),
                (1200.0, 700.0),
                (900.0, 200.0),
            ] {
                let ray = lens.unproject(u, v);
                let back = lens.project(ray.map(|c| c * 3.0)).unwrap();
                assert!(
                    (back[0] - u).abs() < 1e-3 && (back[1] - v).abs() < 1e-3,
                    "{model:?} {u},{v} -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn rotations_and_quaternions_agree() {
        let q = [0.9, 0.1, -0.3, 0.2];
        let n = (q.iter().map(|v: &f64| v * v).sum::<f64>()).sqrt();
        let q = q.map(|v| v / n);
        let m = from_quaternion(q);
        let back = to_quaternion(&m);
        for i in 0..4 {
            assert!(close(q[i], back[i]), "{q:?} {back:?}");
        }
        // A quarter turn about Z takes X to Y.
        let h = std::f64::consts::FRAC_1_SQRT_2;
        let v = mul(&from_quaternion([h, 0.0, 0.0, h]), [1.0, 0.0, 0.0]);
        assert!(close(v[0], 0.0) && close(v[1], 1.0));
    }
}
