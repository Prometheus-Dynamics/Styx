//! The neutral ISP settings the loop hands out, and how the algorithms' parameters become them.

use alloc::vec;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};
use styx_aaa::{
    DenoiseParams, IDENTITY, LensShading, Matrix3, Params, Pwl, SharpenParams, ZoneGrid,
};

/// What the ISP applies to a frame: everything is normalised (full scale 1.0) and hardware
/// independent. The ISP's own mapping turns these into its registers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IspSettings {
    /// The frame whose statistics produced these settings.
    pub from_frame: u64,
    /// Black level to subtract (the smallest of the per-channel levels).
    pub black_level: f64,
    /// Per-channel black levels (red, green on the red row, green on the blue row, blue), when
    /// they are not all equal (see [`Self::black_cfa`]). `None`: every channel is `black_level`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub black_channels: Option<[f64; 4]>,
    /// White balance gains, green 1.
    pub wb: [f64; 3],
    /// Digital gain on top of the white balance (exposure the sensor could not provide, the
    /// difference while a new exposure is still on its way, and deflicker's correction).
    pub digital_gain: f64,
    /// How much brighter than the mean light the flicker made the frame, as deflicker corrects
    /// it (1: no correction). Temporal denoise scales its average by it.
    #[serde(default = "one")]
    pub flicker: f64,
    /// Rolling shutter: deflicker's gains per band of rows (top to bottom), folded into
    /// `lens_shading` (see [`crate::Controller`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flicker_bands: Option<Vec<f64>>,
    /// Colour correction matrix (row-major, camera RGB to output RGB after white balance).
    pub ccm: Matrix3,
    /// Tone curve on `[0, 1]`; `None` is the sRGB curve.
    pub gamma: Option<Pwl>,
    /// Lens shading gains, if the tuning calibrates them.
    pub lens_shading: Option<LensShading>,
    /// Denoise, green equalisation and defective pixel correction (ISPs with those blocks).
    #[serde(default)]
    pub denoise: DenoiseParams,
    /// Sharpening factors (`None`: the ISP's default).
    pub sharpen: Option<SharpenParams>,
    /// Zone weights of the luma histogram (the metering mode's; `None`: uniform).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram_weights: Option<ZoneGrid<f64>>,
}

fn one() -> f64 {
    1.0
}

impl IspSettings {
    /// Neutral settings: black level only, unity gains, identity matrix, sRGB tone.
    pub fn neutral(black_level: f64) -> Self {
        Self {
            from_frame: 0,
            black_level,
            black_channels: None,
            wb: [1.0; 3],
            digital_gain: 1.0,
            flicker: 1.0,
            flicker_bands: None,
            ccm: IDENTITY,
            gamma: None,
            lens_shading: None,
            denoise: DenoiseParams::default(),
            sharpen: None,
            histogram_weights: None,
        }
    }

    /// The black level of each CFA channel (red, green on the red row, green on the blue row,
    /// blue), normalised.
    pub fn black_cfa(&self) -> [f64; 4] {
        self.black_channels.unwrap_or([self.black_level; 4])
    }

    /// White balance times digital gain, per channel, with an extra gain of 1 / the smallest
    /// white balance gain when that is below 1, so no channel gets less than unity and saturated
    /// pixels stay white instead of turning cyan or magenta.
    pub fn channel_gains(&self) -> [f64; 3] {
        let min = self.wb.iter().copied().fold(1.0, f64::min).max(0.1);
        self.wb.map(|w| w * self.digital_gain / min)
    }

    /// Sets these settings to what `p` (computed from frame `frame`) asks for, with
    /// `digital_gain` for the frame being processed. Reuses this settings' curve and table
    /// buffers.
    pub fn set_from_params(&mut self, p: &Params, frame: u64, digital_gain: f64) {
        let g = p.colour_gains[1].max(1e-6);
        // Every field is set here (a field left out would keep the old frame's value).
        let cfa = p.black_level.cfa();
        self.from_frame = frame;
        self.black_level = cfa.into_iter().fold(f64::INFINITY, f64::min);
        self.black_channels = cfa.iter().any(|v| *v != cfa[0]).then_some(cfa);
        self.wb = [p.colour_gains[0] / g, 1.0, p.colour_gains[2] / g];
        self.digital_gain = digital_gain * g;
        self.flicker = 1.0;
        self.flicker_bands = None;
        self.ccm = p.ccm;
        self.gamma.clone_from(&p.gamma);
        self.lens_shading.clone_from(&p.lens_shading);
        self.denoise = p.denoise;
        self.sharpen = p.sharpen;
        self.histogram_weights.clone_from(&p.histogram_weights);
    }

    /// Scales the spatial (SDN) and colour (CDN) denoise thresholds by `scale` (both off at 0;
    /// 1 leaves them as tuned).
    pub fn scale_spatial_denoise(&mut self, scale: f64) {
        if scale == 1.0 {
            return;
        }
        let d = &mut self.denoise;
        if scale <= 0.0 {
            (d.sdn, d.cdn) = (None, None);
        }
        if let Some(s) = &mut d.sdn {
            s.noise_constant *= scale;
            s.noise_slope *= scale;
            s.noise_constant2 *= scale;
            s.noise_slope2 *= scale;
        }
        if let Some(c) = &mut d.cdn {
            c.threshold *= scale;
        }
    }
}

/// The lens shading table with deflicker's band gains folded in: one gain per band of rows
/// (top to bottom), interpolated linearly between band centres and applied to every row of the
/// table. Without a table, a unity table of the bands' height.
pub(crate) fn lens_shading_with_bands(ls: Option<&LensShading>, bands: &[f64]) -> LensShading {
    let mut out = ls.cloned().unwrap_or_else(|| {
        let n = 2 * bands.len().max(2);
        LensShading {
            width: 2,
            height: bands.len().max(2) as u32,
            r: vec![1.0; n],
            g: vec![1.0; n],
            b: vec![1.0; n],
            generation: 0,
        }
    });
    let (w, h) = (out.width as usize, out.height as usize);
    if bands.is_empty() || w == 0 || h == 0 {
        return out;
    }
    // The gains change below: these are no longer `ls`'s tables.
    out.generation = 0;
    for y in 0..h {
        // The band gain at the grid row's centre, linearly between band centres.
        let pos = ((y as f64 + 0.5) / h as f64 * bands.len() as f64 - 0.5)
            .clamp(0.0, (bands.len() - 1) as f64);
        let i = (pos as usize).min(bands.len() - 1);
        let next = bands[(i + 1).min(bands.len() - 1)];
        let g = bands[i] + (next - bands[i]) * (pos - i as f64);
        for t in [&mut out.r, &mut out.g, &mut out.b] {
            for v in t.iter_mut().skip(y * w).take(w) {
                *v *= g;
            }
        }
    }
    out
}
