//! The back end configuration for a stream of frames: [`IspSettings`] mapped into the Lemnos
//! library's [`BackEnd`] and prepared into the tiles config its jobs carry.
//!
//! The geometry (input, the two outputs, the temporal denoise buffers) is fixed when the builder
//! is made. Each frame's settings are applied to a copy of that template, which is cheap (the
//! template holds no heap), and prepared into the builder's own config buffer, so a frame after
//! the first allocates nothing here.

use styx_aaa::Pwl;
use styx_settings::IspSettings;

use crate::PispError;
use styx_lemnos::pisp::be::BackEnd;
use styx_lemnos::pisp::be::lsc::{pack_lut, resample_table};
use styx_lemnos::pisp::be::{SplitJobs, split_tiles};
use styx_lemnos::pisp::uapi::{
    BayerOrder, BeCdnConfig, BeCropConfig, BeDpcConfig, BeGeqConfig, BeLscExtra,
    BeOutputFormatConfig, BeResampleExtra, BeSdnConfig, BeTdnConfig, BeTilesConfig, Branch,
    ImageFormatConfig, rgb_enable,
};

/// The frame geometry the back end is set up for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The raw input the front end gives (16-bit Bayer, the stride the front end writes).
    pub input: ImageFormatConfig,
    pub bayer: BayerOrder,
    /// Output 0 (full size) and output 1 (half size, made by the 2x2 box).
    pub outputs: [ImageFormatConfig; 2],
    /// The temporal denoise buffers' format (the input's), when the buffers exist.
    pub tdn: Option<ImageFormatConfig>,
    /// The input row a frame is split at into two back-end jobs (see [`BackEndBuilder::split`]),
    /// `None` for one job per frame. The config is then prepared in horizontal stripes so the
    /// split can be made.
    pub slice_at: Option<u16>,
}

/// Stripes are cut this many rows above the split line, so the first stripe's input ends by it.
const STRIPE_MARGIN: u16 = 32;

/// Temporal denoise restarts its average above this exposure ratio between frames (the
/// ratio register holds up to 4).
const TDN_MAX_RATIO: f64 = 4.0;

/// See the [module documentation](self).
#[derive(Debug)]
pub struct BackEndBuilder {
    template: BackEnd,
    config: Box<BeTilesConfig>,
    tdn_on: bool,
    /// Exposure of the last frame that read temporal denoise (`None`: the next starts over).
    tdn_last: Option<f64>,
}

impl BackEndBuilder {
    /// The builder for `geometry`. Fails if the library refuses the configuration (before any
    /// frame is sent).
    pub fn new(geometry: &Geometry) -> Result<Self, PispError> {
        let [out0, out1] = geometry.outputs;
        let mut be = BackEnd::simple_bayer(
            geometry.input,
            geometry.bayer,
            0,
            (1.0, 1.0, 1.0),
            None,
            out0.format,
            None,
        );
        for (i, out) in geometry.outputs.iter().enumerate() {
            let branch = branch(i);
            be.set_output_format(
                branch,
                BeOutputFormatConfig {
                    image: *out,
                    ..Default::default()
                },
            );
            if (out.width, out.height) != (geometry.input.width, geometry.input.height) {
                be.set_smart_resize(branch, out.width, out.height);
            }
            let rgb = be.config().global.rgb_enables | rgb_enable::output(i);
            be.config_mut().global.rgb_enables = rgb;
            be.set_crop(branch, BeCropConfig::default());
        }
        if (u32::from(out1.width) * 2, u32::from(out1.height) * 2)
            == (
                u32::from(geometry.input.width),
                u32::from(geometry.input.height),
            )
        {
            set_box_half(&mut be);
        }
        if let Some(f) = geometry.tdn {
            be.set_tdn_format(f);
        }
        if let Some(line) = geometry.slice_at {
            if line == 0 || u32::from(line) >= u32::from(geometry.input.height) {
                return Err(PispError::Config(format!(
                    "slice point row {line} is outside the {} rows of the frame",
                    geometry.input.height
                )));
            }
            be.set_max_stripe_height(line.saturating_sub(STRIPE_MARGIN));
        }
        let template = be;
        let mut config = Box::<BeTilesConfig>::default();
        template
            .clone()
            .prepare_into(&mut config)
            .map_err(prepare_error)?;
        Ok(Self {
            template,
            config,
            tdn_on: geometry.tdn.is_some(),
            tdn_last: None,
        })
    }

    /// The config as of the last [`Self::update`] (the template's before the first).
    pub fn config(&self) -> &BeTilesConfig {
        &self.config
    }

    /// Lets temporal denoise start its average over with the next frame.
    pub fn reset_tdn(&mut self) {
        self.tdn_last = None;
    }

    /// Maps `settings` for a frame exposed for `exposure` (exposure time × analogue gain ×
    /// flicker correction, in any consistent unit) onto the config. Temporal denoise scales its
    /// average by the exposure ratio to the previous frame that read it.
    pub fn update(&mut self, settings: &IspSettings, exposure: f64) -> Result<(), PispError> {
        let tdn = self.tdn_for(settings, exposure);
        let mut be = self.template.clone();
        apply(&mut be, settings)?;
        match tdn {
            Some((ratio, read)) => {
                let t = settings.denoise.tdn.expect("temporal denoise settings");
                let cfg = BeTdnConfig {
                    ratio: field(ratio, 16, 14),
                    noise_constant: field(t.noise_constant, 16, 0),
                    noise_slope: field(t.noise_slope, 16, 8),
                    threshold: field(t.threshold, 16, 16),
                    reset: u8::from(!read),
                    ..Default::default()
                };
                be.set_tdn(Some(cfg), read);
            }
            None => be.set_tdn(None, false),
        }
        be.prepare_into(&mut self.config).map_err(prepare_error)?;
        Ok(())
    }

    /// The temporal denoise ratio and whether the frame reads the average, or `None` when the
    /// frame has no temporal denoise.
    fn tdn_for(&mut self, settings: &IspSettings, exposure: f64) -> Option<(f64, bool)> {
        let exposure =
            Some(exposure).filter(|e| *e > 0.0 && self.tdn_on && settings.denoise.tdn.is_some());
        let Some(e) = exposure else {
            self.tdn_last = None;
            return None;
        };
        let ratio = self.tdn_last.map(|last| e / last);
        self.tdn_last = Some(e);
        match ratio {
            Some(r) if r < TDN_MAX_RATIO => Some((r, true)),
            _ => Some((1.0, false)),
        }
    }

    /// The frame split at row `line`: the first job runs once the rows up to `line` are in, the
    /// rest at the frame's end. `None` when the library cannot split it there.
    pub fn split(&self, line: u32) -> Option<SplitJobs> {
        split_tiles(&self.config, line)
    }
}

/// The library's branch for output `i`.
fn branch(i: usize) -> Branch {
    if i == 0 { Branch::Zero } else { Branch::One }
}

fn prepare_error(e: styx_lemnos::pisp::be::PrepareError) -> PispError {
    PispError::Config(format!("{e}"))
}

/// Applies `s` to `be`: the blocks the algorithms move, in the order that makes the black level
/// the denoise blocks read.
fn apply(be: &mut BackEnd, s: &IspSettings) -> Result<(), PispError> {
    be.set_black_levels(s.black_cfa().map(level16));
    let d = &s.denoise;
    if d.dpc != 0 {
        return Err(PispError::Unsupported(
            "defective pixel correction level (denoise.dpc)",
        ));
    }
    be.set_dpc(None::<BeDpcConfig>);
    be.set_geq(d.geq.map(|g| BeGeqConfig {
        offset: field(g.offset, 16, 0),
        slope_sharper: field(g.slope, 10, 10),
        min: 0,
        max: 0xffff,
    }));
    be.set_sdn(d.sdn.map(|s| BeSdnConfig {
        leakage: field(1.0 - s.strength, 8, 8) as u8,
        noise_constant: field(s.noise_constant, 16, 0),
        noise_slope: field(s.noise_slope, 16, 8),
        noise_constant2: field(s.noise_constant2, 16, 0),
        noise_slope2: field(s.noise_slope2, 16, 8),
        ..Default::default()
    }));
    be.set_cdn(d.cdn.map(|c| BeCdnConfig {
        thresh: field(c.threshold, 16, 0),
        iir_strength: field(c.strength, 8, 8) as u8,
        g_adjust: 0,
    }));
    if s.sharpen.is_some() {
        return Err(PispError::Unsupported(
            "sharpening (the library has no sharpen defaults or scaling)",
        ));
    }
    if let Some(ls) = &s.lens_shading {
        let (w, h) = (ls.width as usize, ls.height as usize);
        let lsc =
            |t: &[f64]| resample_table(t, w, h).map_err(|e| PispError::Config(format!("{e}")));
        let table = [lsc(&ls.r)?, lsc(&ls.g)?, lsc(&ls.b)?];
        be.set_lsc(pack_lut(&table), BeLscExtra::default());
    }
    let g = s.channel_gains();
    be.set_wb_gains(g[0], g[1], g[2]);
    be.set_ccm(s.ccm);
    be.set_gamma_curve(&gamma_points(s.gamma.as_ref()));
    // The tone curve is always on: the library's template leaves its block enable off.
    let rgb = be.config().global.rgb_enables | rgb_enable::GAMMA;
    be.config_mut().global.rgb_enables = rgb;
    Ok(())
}

/// A normalised level on the 16-bit scale the PiSP works in.
pub fn level16(v: f64) -> u16 {
    (v * 65536.0).round().clamp(0.0, 65535.0) as u16
}

/// `v` as an unsigned fixed-point register field of `bits` bits with `frac` fractional bits,
/// rounded and clamped.
fn field(v: f64, bits: u32, frac: u32) -> u16 {
    let max = f64::from((1u32 << bits) - 1);
    (v * f64::from(1u32 << frac)).round().clamp(0.0, max) as u16
}

/// The tone curve as `(x, y)` points on 16-bit scales; the sRGB curve (65 points) when `None`.
pub fn gamma_points(curve: Option<&Pwl>) -> Vec<(u32, u32)> {
    let to16 = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u32;
    match curve {
        Some(p) if p.points().len() >= 2 => p
            .points()
            .iter()
            .map(|&(x, y)| (to16(x), to16(y)))
            .collect(),
        _ => (0..=64)
            .map(|i| {
                let x = (f64::from(i) / 64.0).powi(2);
                let y = if x <= 0.003_130_8 {
                    12.92 * x
                } else {
                    1.055 * x.powf(1.0 / 2.4) - 0.055
                };
                (to16(x), to16(y))
            })
            .collect(),
    }
}

/// Output 1 as a 2x2 box average of output 1's input: the resampler at a step of exactly 2 with
/// two equal taps (`[0, 0, 512, 512, 0, 0]` per phase), as the library's `box2` and the CPU's
/// half-size filter.
fn set_box_half(be: &mut BackEnd) {
    let out = be.config().output_format[1].image;
    let (w, h) = (out.width, out.height);
    be.set_resample_step(Branch::One, None);
    be.set_smart_resize(Branch::One, 0, 0);
    let mut rgb =
        be.config().global.rgb_enables & !(rgb_enable::downscale(1) | rgb_enable::resample(1));
    be.set_resample(
        Branch::One,
        BeResampleExtra {
            scaled_width: w,
            scaled_height: h,
            ..Default::default()
        },
    );
    let step = 2u16 << 12;
    be.set_resample_step(Branch::One, Some((step, step)));
    be.set_resample_filter(Branch::One, &styx_lemnos::pisp::be::ops::BOX2);
    rgb |= rgb_enable::resample(1);
    be.config_mut().global.rgb_enables = rgb;
}
