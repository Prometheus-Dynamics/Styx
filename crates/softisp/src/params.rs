//! Pipeline parameters: plain data, serde-able, one optional block per stage.

use serde::{Deserialize, Serialize};

/// Every stage of the pipeline, in processing order. `None` skips a stage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IspParams {
    /// Subtracted from each sample, per CFA cell, in input sample units.
    pub black_level: Option<BlackLevel>,
    /// Per-channel gains (white balance).
    pub white_balance: Option<WhiteBalance>,
    /// A gain for every channel, applied with the white balance.
    pub digital_gain: f32,
    /// Spatially varying per-channel gains from a coarse grid.
    pub lens_shading: Option<LensShading>,
    pub demosaic: Demosaic,
    /// Colour correction from camera RGB to output RGB, in linear light.
    pub ccm: Option<ColorMatrix>,
    /// Tone / gamma curve from linear light to 8-bit output. `None`: linear.
    pub tone: Option<ToneCurve>,
    /// RGB to YCbCr matrix of the YUV outputs.
    pub yuv: YuvMatrix,
    /// 3A statistics gathered in the same pass.
    pub stats: Option<StatsConfig>,
    /// The arithmetic of the per-pixel stages.
    pub arithmetic: Arithmetic,
}

impl Default for IspParams {
    fn default() -> Self {
        Self {
            black_level: None,
            white_balance: None,
            digital_gain: 1.0,
            lens_shading: None,
            demosaic: Demosaic::Bilinear,
            ccm: None,
            tone: None,
            yuv: YuvMatrix::Bt709Limited,
            stats: None,
            arithmetic: Arithmetic::Auto,
        }
    }
}

/// How the per-pixel stages compute. The two give pictures within a code or two of each
/// other (see `PERFORMANCE.md` for measured differences); [`SoftIsp::arithmetic`](crate::SoftIsp::arithmetic) tells which one runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arithmetic {
    /// [`Self::Half`] where it is fast and applies (with a colour matrix or a tone curve, on
    /// CPUs with FP16 arithmetic), else [`Self::Int`].
    #[default]
    Auto,
    /// 12-bit fixed point: the reference, on every CPU (SIMD on x86 and AArch64).
    Int,
    /// fp16 in the front end, the bilinear demosaic, the colour matrix and the tone curve
    /// (whose 257-node table becomes 48 segments looked up by the fp16 exponent); 2-3 times
    /// faster on CPUs with FP16 arithmetic (ARMv8.2 and later: Cortex-A55, A76, ...), emulated
    /// (slowly) elsewhere. Applies to inputs of 10 bits or fewer, the bilinear demosaic and
    /// tone curves that never fall; other set-ups use [`Self::Int`].
    Half,
}

/// Black level per CFA cell: red, green on red rows, green on blue rows, blue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlackLevel {
    pub r: u16,
    pub gr: u16,
    pub gb: u16,
    pub b: u16,
}

impl BlackLevel {
    pub const fn uniform(level: u16) -> Self {
        Self {
            r: level,
            gr: level,
            gb: level,
            b: level,
        }
    }

    /// In [`crate::CfaPattern::cell_at`] order.
    pub fn cells(&self) -> [u16; 4] {
        [self.r, self.gr, self.gb, self.b]
    }
}

/// Channel gains (1.0 = unchanged). The product with the digital gain and the lens shading
/// gain is limited to 16.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WhiteBalance {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl WhiteBalance {
    pub fn gains(&self) -> [f32; 3] {
        [self.r, self.g, self.b]
    }
}

/// A `width` x `height` grid of gains per channel (row-major), spread evenly over the frame
/// with the outer nodes on the corner pixels, interpolated bilinearly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LensShading {
    pub width: u32,
    pub height: u32,
    pub r: Vec<f32>,
    pub g: Vec<f32>,
    pub b: Vec<f32>,
}

impl LensShading {
    /// A radial falloff correction: gain `1 + strength * (r / r_corner)^2` on every channel.
    pub fn radial(width: u32, height: u32, strength: f32) -> Self {
        let grid: Vec<f32> = (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    let fx = 2.0 * x as f32 / (width.max(2) - 1) as f32 - 1.0;
                    let fy = 2.0 * y as f32 / (height.max(2) - 1) as f32 - 1.0;
                    1.0 + strength * (fx * fx + fy * fy) / 2.0
                })
            })
            .collect();
        Self {
            width,
            height,
            r: grid.clone(),
            g: grid.clone(),
            b: grid,
        }
    }
}

/// How missing colours are interpolated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Demosaic {
    /// Mean of the nearest samples of each colour (3x3).
    #[default]
    Bilinear,
    /// Malvar-He-Cutler: bilinear corrected by the local gradient of the sampled colour (5x5).
    /// Much less colour fringing and zipper on edges; about 1.8 ms more per 1280x800 frame on
    /// a Cortex-A76.
    Mhc,
}

/// A 3x3 matrix; row `k` gives output channel `k` (R, G, B) from input R, G, B. Coefficients
/// must lie within ±4 (applied in Q12).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorMatrix {
    pub m: [[f32; 3]; 3],
}

impl ColorMatrix {
    pub const IDENTITY: Self = Self {
        m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    };

    /// Identity blended with a saturation change keeping luma (`1.0` = unchanged).
    pub fn saturation(s: f32) -> Self {
        let l = [0.2126f32, 0.7152, 0.0722];
        let mut m = [[0.0; 3]; 3];
        for (k, row) in m.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (1.0 - s) * l[j] + if j == k { s } else { 0.0 };
            }
        }
        Self { m }
    }
}

/// Linear light (0..1) to output code values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToneCurve {
    /// `out = in^(1 / gamma)`.
    Gamma { gamma: f32 },
    /// The sRGB transfer function.
    Srgb,
    /// Piecewise linear through `[input, output]` points in 0..1, sorted by input.
    Points { points: Vec<[f32; 2]> },
}

impl ToneCurve {
    /// The curve at `x` in 0..1.
    pub fn eval(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Self::Gamma { gamma } => x.powf(1.0 / gamma.max(1e-3)),
            Self::Srgb => {
                if x <= 0.003_130_8 {
                    12.92 * x
                } else {
                    1.055 * x.powf(1.0 / 2.4) - 0.055
                }
            }
            Self::Points { points } => {
                let Some(first) = points.first() else {
                    return x;
                };
                if x <= first[0] {
                    return first[1];
                }
                // The first segment whose end is at or after `x` (a binary search: the curve
                // is sampled a few thousand times whenever it changes, which adaptive contrast
                // makes it do on most frames).
                let k = points[1..].partition_point(|p| p[0] < x);
                let Some(&[x1, y1]) = points.get(k + 1) else {
                    return points[points.len() - 1][1];
                };
                let [x0, y0] = points[k];
                let t = if x1 > x0 { (x - x0) / (x1 - x0) } else { 1.0 };
                y0 + t * (y1 - y0)
            }
        }
    }
}

/// RGB to YCbCr conversion of the YUV outputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YuvMatrix {
    /// BT.601, full range (JFIF / sYCC).
    Bt601Full,
    /// BT.709, limited range (video).
    #[default]
    Bt709Limited,
}

/// Which statistics to gather, on 2x2 quads of the mosaic after black level, gains and lens
/// shading (see [`crate::IspStats`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatsConfig {
    /// Zones across and down the frame.
    pub zones_x: u32,
    pub zones_y: u32,
    /// Luma histogram bins (1 to 4096).
    pub histogram_bins: u32,
    /// Quads with any channel at or above this fraction of full scale are left out of the
    /// colour sums (clipped colours mislead white balance).
    pub saturation: f32,
    /// Sample every `row_step`-th quad row (1: all).
    pub row_step: u32,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            zones_x: 16,
            zones_y: 12,
            histogram_bins: 256,
            saturation: 0.95,
            row_step: 1,
        }
    }
}
