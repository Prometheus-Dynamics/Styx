//! Colour matrices for DNG: `ColorMatrix` (XYZ → camera) and `ForwardMatrix` (white-balanced
//! camera → XYZ D50) from an ISP's camera → sRGB CCM, and back. See the crate documentation
//! for the derivation.

/// A 3x3 row-major matrix.
pub type Matrix3 = [f64; 9];

/// The identity.
pub const IDENTITY: Matrix3 = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// Linear sRGB → XYZ (D65 white, IEC 61966-2-1).
pub const SRGB_TO_XYZ: Matrix3 = [
    0.412_456_4,
    0.357_576_1,
    0.180_437_5,
    0.212_672_9,
    0.715_152_2,
    0.072_175_0,
    0.019_333_9,
    0.119_192_0,
    0.950_304_1,
];

/// Bradford cone response matrix.
const BRADFORD: Matrix3 = [
    0.8951, 0.2664, -0.1614, -0.7502, 1.7135, 0.0367, 0.0389, -0.0685, 1.0296,
];

/// D50 white (xy), the profile connection space of `ForwardMatrix`.
pub const D50_XY: (f64, f64) = (0.3457, 0.3585);
/// D65 white (xy), sRGB's.
pub const D65_XY: (f64, f64) = (0.312_7, 0.329_0);

/// XYZ of sRGB's white (D65 as the sRGB matrix has it: `SRGB_TO_XYZ · (1, 1, 1)`).
pub fn srgb_white() -> [f64; 3] {
    mul_vec(&SRGB_TO_XYZ, [1.0; 3])
}

/// `a · b`.
pub fn mul(a: &Matrix3, b: &Matrix3) -> Matrix3 {
    std::array::from_fn(|i| {
        let (r, c) = (i / 3, i % 3);
        (0..3).map(|k| a[r * 3 + k] * b[k * 3 + c]).sum()
    })
}

/// `m · v`.
pub fn mul_vec(m: &Matrix3, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|r| (0..3).map(|k| m[r * 3 + k] * v[k]).sum())
}

/// The diagonal matrix of `v`.
pub fn diag(v: [f64; 3]) -> Matrix3 {
    [v[0], 0.0, 0.0, 0.0, v[1], 0.0, 0.0, 0.0, v[2]]
}

/// The inverse, if `m` is invertible.
pub fn inverse(m: &Matrix3) -> Option<Matrix3> {
    let [a, b, c, d, e, f, g, h, i] = *m;
    let co = [
        e * i - f * h,
        c * h - b * i,
        b * f - c * e,
        f * g - d * i,
        a * i - c * g,
        c * d - a * f,
        d * h - e * g,
        b * g - a * h,
        a * e - b * d,
    ];
    let det = a * co[0] + b * co[3] + c * co[6];
    if det.abs() < 1e-12 {
        return None;
    }
    Some(co.map(|v| v / det))
}

/// XYZ (Y = 1) of chromaticity `xy`.
pub fn xy_to_xyz((x, y): (f64, f64)) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Bradford chromatic adaptation from white `from` to white `to` (XYZ).
pub fn bradford(from: [f64; 3], to: [f64; 3]) -> Matrix3 {
    let s = mul_vec(&BRADFORD, from);
    let d = mul_vec(&BRADFORD, to);
    let scale = diag([d[0] / s[0], d[1] / s[1], d[2] / s[2]]);
    let inv = inverse(&BRADFORD).unwrap_or(IDENTITY);
    mul(&inv, &mul(&scale, &BRADFORD))
}

/// Chromaticity of a light of correlated colour temperature `cct` (kelvin): the Planckian
/// locus below 4000 K (Kim et al.'s cubic fit), the CIE daylight locus from there.
pub fn cct_to_xy(cct: f64) -> (f64, f64) {
    let t = cct.clamp(1667.0, 25000.0);
    if t < 4000.0 {
        let x =
            -0.266_123_9e9 / t.powi(3) - 0.234_358_9e6 / t.powi(2) + 0.877_695_6e3 / t + 0.179_910;
        let y = if t < 2222.0 {
            -1.106_381_4 * x.powi(3) - 1.348_110_20 * x.powi(2) + 2.185_558_32 * x - 0.202_196_83
        } else {
            -0.954_947_6 * x.powi(3) - 1.374_185_93 * x.powi(2) + 2.091_370_15 * x - 0.167_488_67
        };
        (x, y)
    } else {
        let x = if t <= 7000.0 {
            -4.6070e9 / t.powi(3) + 2.9678e6 / t.powi(2) + 0.09911e3 / t + 0.244063
        } else {
            -2.0064e9 / t.powi(3) + 1.9018e6 / t.powi(2) + 0.24748e3 / t + 0.237040
        };
        (x, -3.0 * x * x + 2.87 * x - 0.275)
    }
}

/// DNG calibration illuminants (EXIF `LightSource` values) with a defined white.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Illuminant {
    /// CIE standard illuminant A (tungsten, 2856 K), code 17.
    StandardA,
    /// ISO studio tungsten (3200 K), code 24.
    IsoStudioTungsten,
    /// D50, code 23.
    D50,
    /// D55, code 20.
    D55,
    /// D65, code 21.
    D65,
    /// D75, code 22.
    D75,
}

impl Illuminant {
    /// The `CalibrationIlluminant` / `LightSource` code.
    pub fn code(self) -> u16 {
        match self {
            Self::StandardA => 17,
            Self::IsoStudioTungsten => 24,
            Self::D50 => 23,
            Self::D55 => 20,
            Self::D65 => 21,
            Self::D75 => 22,
        }
    }

    /// The illuminant of a code, if it is one of these.
    pub fn from_code(code: u16) -> Option<Self> {
        [
            Self::StandardA,
            Self::IsoStudioTungsten,
            Self::D50,
            Self::D55,
            Self::D65,
            Self::D75,
        ]
        .into_iter()
        .find(|i| i.code() == code)
    }

    /// Correlated colour temperature (kelvin).
    pub fn cct(self) -> f64 {
        match self {
            Self::StandardA => 2856.0,
            Self::IsoStudioTungsten => 3200.0,
            Self::D50 => 5003.0,
            Self::D55 => 5503.0,
            Self::D65 => 6504.0,
            Self::D75 => 7504.0,
        }
    }

    /// White chromaticity.
    pub fn xy(self) -> (f64, f64) {
        match self {
            Self::StandardA => (0.447_57, 0.407_45),
            Self::IsoStudioTungsten => cct_to_xy(3200.0),
            Self::D50 => D50_XY,
            Self::D55 => (0.332_42, 0.347_43),
            Self::D65 => D65_XY,
            Self::D75 => (0.299_02, 0.315_02),
        }
    }
}

/// Camera RGB → XYZ under a light of white `white_xy`, for an ISP that white-balances with
/// `1 / neutral` and then applies `ccm` (camera → linear sRGB, adapted to D65):
/// `Bradford(D65 → white) · SRGB_TO_XYZ · ccm · diag(1 / neutral)`.
pub fn camera_to_xyz(ccm: &Matrix3, neutral: [f64; 3], white_xy: (f64, f64)) -> Matrix3 {
    let adapt = bradford(srgb_white(), xy_to_xyz(white_xy));
    let wb = diag(neutral.map(|n| 1.0 / n.max(1e-9)));
    mul(&adapt, &mul(&SRGB_TO_XYZ, &mul(ccm, &wb)))
}

/// DNG `ColorMatrix` (XYZ → camera) for a calibration light of white `white_xy` at which the
/// camera's neutral is `neutral` and the ISP's CCM is `ccm`: the inverse of
/// [`camera_to_xyz`], scaled so the light's white maps to a camera colour whose largest
/// component is 1.
pub fn color_matrix(ccm: &Matrix3, neutral: [f64; 3], white_xy: (f64, f64)) -> Option<Matrix3> {
    let cm = inverse(&camera_to_xyz(ccm, neutral, white_xy))?;
    let cam = mul_vec(&cm, xy_to_xyz(white_xy));
    let max = cam.iter().copied().fold(f64::MIN, f64::max);
    (max > 0.0).then(|| cm.map(|v| v / max))
}

/// DNG `ForwardMatrix` (white-balanced camera → XYZ D50) for an ISP CCM:
/// `Bradford(D65 → D50) · SRGB_TO_XYZ · ccm`, each row scaled so `(1, 1, 1)` maps to D50.
pub fn forward_matrix(ccm: &Matrix3) -> Matrix3 {
    let d50 = xy_to_xyz(D50_XY);
    let adapt = bradford(srgb_white(), d50);
    let mut fm = mul(&adapt, &mul(&SRGB_TO_XYZ, ccm));
    let white = mul_vec(&fm, [1.0; 3]);
    for r in 0..3 {
        if white[r].abs() > 1e-9 {
            for c in 0..3 {
                fm[r * 3 + c] *= d50[r] / white[r];
            }
        }
    }
    fm
}

/// Back from a `ColorMatrix` calibrated under a light of white `white_xy` to the camera's
/// neutral under that light (green 1) and the CCM (camera → linear sRGB after white balance)
/// an ISP would use there: the inverse of [`color_matrix`] (exact for CCMs whose rows sum to 1).
pub fn ccm_from_color_matrix(
    color_matrix: &Matrix3,
    white_xy: (f64, f64),
) -> Option<(Matrix3, [f64; 3])> {
    let w = xy_to_xyz(white_xy);
    let n = mul_vec(color_matrix, w);
    if n[1] <= 0.0 {
        return None;
    }
    let neutral = n.map(|v| v / n[1]);
    let to_xyz = inverse(color_matrix)?;
    let adapt = bradford(w, srgb_white());
    let srgb = inverse(&SRGB_TO_XYZ)?;
    let mut ccm = mul(&srgb, &mul(&adapt, &mul(&to_xyz, &diag(neutral))));
    // `to_xyz · neutral` is the light's white times a scale; make white map to white.
    let white = mul_vec(&ccm, [1.0; 3]);
    let s = (white[0] + white[1] + white[2]) / 3.0;
    if s.abs() > 1e-9 {
        ccm = ccm.map(|v| v / s);
    }
    Some((ccm, neutral))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &[f64], b: &[f64], tol: f64) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < tol)
    }

    /// A Raspberry Pi tuning's CCM (rows sum to 1).
    const CCM: Matrix3 = [
        1.80439, -0.73699, -0.06739, -0.36073, 1.83327, -0.47255, -0.08378, -0.56403, 1.64781,
    ];

    #[test]
    fn inverse_and_bradford() {
        let m = inverse(&CCM).unwrap();
        assert!(close(&mul(&m, &CCM), &IDENTITY, 1e-12));
        assert!(inverse(&[0.0; 9]).is_none());
        let a = xy_to_xyz(Illuminant::StandardA.xy());
        let d65 = xy_to_xyz(D65_XY);
        assert!(close(&mul_vec(&bradford(d65, a), d65), &a, 1e-9));
        // sRGB white is D65.
        assert!(close(&mul_vec(&SRGB_TO_XYZ, [1.0; 3]), &d65, 1e-3));
    }

    #[test]
    fn colour_temperatures_follow_the_loci() {
        let (x, y) = cct_to_xy(2856.0);
        assert!(
            (x - 0.4476).abs() < 2e-3 && (y - 0.4074).abs() < 2e-3,
            "{x} {y}"
        );
        let (x, y) = cct_to_xy(6504.0);
        assert!(
            (x - 0.3127).abs() < 2e-3 && (y - 0.3290).abs() < 2e-3,
            "{x} {y}"
        );
        assert_eq!(Illuminant::from_code(21), Some(Illuminant::D65));
        assert_eq!(Illuminant::from_code(99), None);
    }

    #[test]
    fn color_matrix_maps_the_light_to_the_neutral_and_round_trips() {
        let neutral = [0.52, 1.0, 0.71];
        for ill in [Illuminant::StandardA, Illuminant::D65] {
            let cm = color_matrix(&CCM, neutral, ill.xy()).unwrap();
            // The light's white lands on the neutral, largest component 1 (the tuning's rows
            // sum to 1 within 1e-5).
            let cam = mul_vec(&cm, xy_to_xyz(ill.xy()));
            assert!(close(&cam, &[0.52, 1.0, 0.71], 1e-4), "{cam:?}");
            let (ccm, n) = ccm_from_color_matrix(&cm, ill.xy()).unwrap();
            assert!(close(&ccm, &CCM, 1e-4), "{ccm:?}");
            assert!(close(&n, &neutral, 1e-4));
            // A grey seen by the camera (the neutral) through the ColorMatrix's inverse is
            // the light's white.
            let xyz = mul_vec(&inverse(&cm).unwrap(), neutral);
            let w = xy_to_xyz(ill.xy());
            assert!(close(&xyz.map(|v| v / xyz[1]), &w, 1e-4));
        }
    }

    #[test]
    fn forward_matrix_maps_white_to_d50_and_srgb_primaries_through() {
        let fm = forward_matrix(&IDENTITY);
        assert!(close(&mul_vec(&fm, [1.0; 3]), &xy_to_xyz(D50_XY), 1e-9));
        // The identity CCM gives the D50-adapted sRGB matrix: red's Y is sRGB's.
        assert!((fm[3] - 0.2225).abs() < 2e-3, "{fm:?}");
        let fm = forward_matrix(&CCM);
        assert!(close(&mul_vec(&fm, [1.0; 3]), &xy_to_xyz(D50_XY), 1e-9));
    }
}
