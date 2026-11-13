//! The neutral ISP settings ([`styx_settings::IspSettings`]) mapped into this ISP's parameters.
//!
//! Normalised values (full scale 1.0) become codes of the raw format's depth; the colour matrix,
//! tone curve, lens shading and denoise/sharpen blocks keep their meaning. What the settings do
//! not carry (the demosaic, the YUV matrix, the arithmetic, the statistics' zones) is in
//! [`Options`].

use alloc::vec::Vec;

use styx_aaa::Matrix3;
use styx_settings::IspSettings;

use crate::format::RawFormat;
use crate::params::{
    Arithmetic, BlackLevel, ChromaDenoise, ColorMatrix, Demosaic, Denoise, IspParams, LensShading,
    LumaDenoise, Paths, Sharpen, StatsConfig, ToneCurve, WhiteBalance, YuvMatrix,
};

/// The choices an ISP makes that the neutral settings do not carry.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub demosaic: Demosaic,
    pub yuv: YuvMatrix,
    pub arithmetic: Arithmetic,
    /// The statistics' zones and histogram; `None` gathers none.
    pub stats: Option<StatsConfig>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            demosaic: Demosaic::Bilinear,
            yuv: YuvMatrix::Bt709Limited,
            arithmetic: Arithmetic::Auto,
            stats: None,
        }
    }
}

/// The ISP parameters for `settings` on `format`, with `options`.
pub fn params_for(format: &RawFormat, settings: &IspSettings, options: &Options) -> IspParams {
    let max = ((1u32 << format.packing.bit_depth()) - 1) as f64;
    let code = |v: f64| (v * max).round().clamp(0.0, max) as u16;
    let [r, gr, gb, b] = settings.black_cfa().map(code);
    IspParams {
        black_level: Some(BlackLevel { r, gr, gb, b }),
        white_balance: Some(WhiteBalance {
            r: settings.wb[0] as f32,
            g: settings.wb[1] as f32,
            b: settings.wb[2] as f32,
        }),
        digital_gain: settings.digital_gain as f32,
        lens_shading: settings.lens_shading.as_ref().map(|l| LensShading {
            width: l.width,
            height: l.height,
            r: to_f32(&l.r),
            g: to_f32(&l.g),
            b: to_f32(&l.b),
        }),
        demosaic: options.demosaic,
        ccm: Some(colour_matrix(&settings.ccm)),
        tone: Some(match &settings.gamma {
            None => ToneCurve::Srgb,
            Some(curve) => ToneCurve::Points {
                points: curve
                    .points()
                    .iter()
                    .map(|&(x, y)| [x as f32, y as f32])
                    .collect(),
            },
        }),
        denoise: denoise(settings),
        sharpen: settings.sharpen.map(|s| Sharpen {
            amount: s.strength as f32,
            threshold: s.threshold as f32,
            limit: s.limit as f32,
        }),
        yuv: options.yuv,
        stats: options.stats,
        arithmetic: options.arithmetic,
        paths: Paths::default(),
    }
}

fn to_f32(values: &[f64]) -> Vec<f32> {
    values.iter().map(|&v| v as f32).collect()
}

fn colour_matrix(m: &Matrix3) -> ColorMatrix {
    let row = |i: usize| [m[i] as f32, m[i + 1] as f32, m[i + 2] as f32];
    ColorMatrix {
        m: [row(0), row(3), row(6)],
    }
}

fn denoise(settings: &IspSettings) -> Option<Denoise> {
    let d = &settings.denoise;
    if d.sdn.is_none() && d.cdn.is_none() {
        return None;
    }
    Some(Denoise {
        luma: d.sdn.map(|s| LumaDenoise {
            constant: s.noise_constant as f32,
            slope: s.noise_slope as f32,
            strength: s.strength as f32,
        }),
        chroma: d.cdn.map(|c| ChromaDenoise {
            threshold: c.threshold as f32,
            strength: c.strength as f32,
        }),
    })
}
