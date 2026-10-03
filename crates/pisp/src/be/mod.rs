//! Back end configuration builder.
//!
//! The back end reads a raw (Bayer or mono) or RGB/YUV image from memory and runs:
//! decompress → DPC → GEQ → TDN → SDN → BLC → stitch → LSC → WBG → CDN → CAC → debin →
//! tonemap → demosaic → CCM → saturation → YCbCr → sharpen/false colour → YCbCr inverse →
//! gamma, then per output branch: crop → CSC → downscale → resample → output format. It works
//! in tiles of at most 640 pixels wide; the tiles are part of the config buffer.
//!
//! The finalisation and "smart resize" rules follow libpisp
//! `src/libpisp/backend/backend.cpp` and `backend_prepare.cpp` (BSD-2-Clause, Copyright (C)
//! 2021 - 2023, Raspberry Pi Ltd), ported to Rust; unlike libpisp, everything is finalised
//! on every [`BackEnd::prepare`] (no dirty tracking) and the tiling is recomputed each time
//! (it takes microseconds).

pub mod defaults;
pub mod lsc;
mod prepare;
pub mod tiling;

use crate::uapi::*;

pub use prepare::PrepareError;

/// Maximum tile width of the BCM2712 back end (libpisp `BackEndMaxTileWidth`).
pub const MAX_TILE_WIDTH: u16 = 640;
/// libpisp's stripe height limit.
pub const MAX_STRIPE_HEIGHT: u16 = 3072;

/// Signed 4.10 fixed point (1.0 = 1024), clamped to the 14-bit field.
pub fn coeff_s4_10(v: f64) -> i16 {
    (v * 1024.0).round().clamp(-8192.0, 8191.0) as i16
}

/// Builds `pisp_be_tiles_config` buffers.
#[derive(Clone, Debug)]
pub struct BackEnd {
    cfg: BeConfig,
    crop: [BeCropConfig; BE_NUM_OUTPUTS],
    downscale_extra: [BeDownscaleExtra; BE_NUM_OUTPUTS],
    resample_extra: [BeResampleExtra; BE_NUM_OUTPUTS],
    smart_resize: [(u16, u16); BE_NUM_OUTPUTS],
    lsc_extra: BeLscExtra,
    max_tile_width: u16,
    max_stripe_height: u16,
}

impl Default for BackEnd {
    fn default() -> Self {
        Self::new()
    }
}

impl BackEnd {
    /// A back end with libpisp's block defaults loaded (debin, demosaic, false colour,
    /// gamma, sharpening, JPEG YCbCr, Lanczos 3 resampling) and nothing enabled.
    pub fn new() -> Self {
        let mut cfg = BeConfig::default();
        cfg.debin.coeffs = defaults::DEBIN_COEFFS;
        cfg.demosaic = defaults::DEMOSAIC;
        cfg.false_colour = defaults::FALSE_COLOUR;
        cfg.gamma = defaults::gamma_from_curve(&defaults::GAMMA_POINTS);
        (cfg.sharpen, cfg.sh_fc_combine) = defaults::sharpen();
        let jpeg = defaults::encoding("jpeg").expect("jpeg encoding");
        cfg.ycbcr = jpeg.ycbcr;
        cfg.ycbcr_inverse = jpeg.inverse;
        for r in &mut cfg.resample {
            r.coef = defaults::LANCZOS3;
        }
        Self {
            cfg,
            crop: Default::default(),
            downscale_extra: Default::default(),
            resample_extra: Default::default(),
            smart_resize: Default::default(),
            lsc_extra: BeLscExtra::default(),
            max_tile_width: MAX_TILE_WIDTH,
            max_stripe_height: MAX_STRIPE_HEIGHT,
        }
    }

    /// The configuration as it stands.
    pub fn config(&self) -> &BeConfig {
        &self.cfg
    }

    /// Mutable access to every block, for settings without a dedicated setter.
    pub fn config_mut(&mut self) -> &mut BeConfig {
        &mut self.cfg
    }

    /// Enables and Bayer order.
    pub fn set_global(&mut self, bayer_enables: u32, rgb_enables: u32, bayer: BayerOrder) {
        self.cfg.global = BeGlobalConfig {
            bayer_enables,
            rgb_enables,
            bayer_order: bayer as u8,
            pad: [0; 3],
        };
    }

    /// Input image format.
    pub fn set_input_format(&mut self, f: ImageFormatConfig) {
        self.cfg.input_format = f;
    }

    /// Black level correction (16-bit scale, one level for all channels).
    pub fn set_black_level(&mut self, level: u16) {
        self.cfg.blc = BlaConfig {
            black_level_r: level,
            black_level_gr: level,
            black_level_gb: level,
            black_level_b: level,
            output_black_level: 0,
            pad: [0; 2],
        };
        self.cfg.sdn.black_level = level;
        self.cfg.tdn.black_level = level;
    }

    /// White balance (and digital) gains.
    pub fn set_wb_gains(&mut self, r: f64, g: f64, b: f64) {
        self.cfg.wbg = WbgConfig {
            gain_r: crate::fe::gain_4_10(r),
            gain_g: crate::fe::gain_4_10(g),
            gain_b: crate::fe::gain_4_10(b),
            pad: [0; 2],
        };
    }

    /// Colour correction matrix (row-major, applied to linear RGB).
    pub fn set_ccm(&mut self, m: [f64; 9]) {
        self.cfg.ccm = BeCcmConfig {
            coeffs: m.map(coeff_s4_10),
            ..Default::default()
        };
    }

    /// Lens shading: the gain table (see [`lsc::pack_lut`]) and where the image starts in it;
    /// enables the block. Grid steps left at zero spread the table over the input image.
    pub fn set_lsc(&mut self, lsc: BeLscConfig, extra: BeLscExtra) {
        self.cfg.lsc = lsc;
        self.lsc_extra = extra;
        self.cfg.global.bayer_enables |= bayer_enable::LSC;
    }

    /// Defective pixel correction: 0 off, 1 normal, 2 strong (as the Raspberry Pi IPA).
    pub fn set_dpc(&mut self, strength: u8) {
        let (cfg, on) = match strength {
            0 => (BeDpcConfig::default(), false),
            1 => (
                BeDpcConfig {
                    coeff_level: 1,
                    coeff_range: 8,
                    ..Default::default()
                },
                true,
            ),
            _ => (BeDpcConfig::default(), true),
        };
        self.cfg.dpc = cfg;
        self.enable_bayer(bayer_enable::DPC, on);
    }

    /// Green equalisation (`None`: off).
    pub fn set_geq(&mut self, geq: Option<BeGeqConfig>) {
        self.cfg.geq = geq.unwrap_or_default();
        self.enable_bayer(bayer_enable::GEQ, geq.is_some());
    }

    /// Spatial denoise (`None`: off); its black level follows [`Self::set_black_level`].
    pub fn set_sdn(&mut self, sdn: Option<BeSdnConfig>) {
        self.cfg.sdn = sdn.unwrap_or_default();
        self.cfg.sdn.black_level = self.cfg.blc.black_level_r;
        self.enable_bayer(bayer_enable::SDN, sdn.is_some());
    }

    /// Colour denoise (`None`: off).
    pub fn set_cdn(&mut self, cdn: Option<BeCdnConfig>) {
        self.cfg.cdn = cdn.unwrap_or_default();
        self.enable_bayer(bayer_enable::CDN, cdn.is_some());
    }

    /// The format of the temporal denoise buffers (the input's: 16-bit Bayer, same size and
    /// stride), needed before [`Self::set_tdn`].
    pub fn set_tdn_format(&mut self, f: ImageFormatConfig) {
        self.cfg.tdn_input_format = f;
        self.cfg.tdn_output_format = f;
    }

    /// Temporal denoise (`None`: off). `input`: read the long-term average written by the
    /// previous job (false after a reset, when there is none). Its black level follows
    /// [`Self::set_black_level`].
    pub fn set_tdn(&mut self, tdn: Option<BeTdnConfig>, input: bool) {
        self.cfg.tdn = tdn.unwrap_or_default();
        self.cfg.tdn.black_level = self.cfg.blc.black_level_r;
        let on = tdn.is_some();
        self.enable_bayer(bayer_enable::TDN | bayer_enable::TDN_OUTPUT, on);
        self.enable_bayer(bayer_enable::TDN_INPUT, on && input);
    }

    /// The default sharpening scaled as the Raspberry Pi IPA scales it: thresholds by
    /// `threshold` / 4 (the tuning's PiSP scale), strengths by `strength`, limits by `limit`.
    pub fn set_sharpen_scaled(&mut self, threshold: f64, strength: f64, limit: f64) {
        let (mut s, shfc) = defaults::sharpen();
        let k = threshold * 0.25;
        let field = |v: u16, x: f64, bits: u32| {
            (f64::from(v) * x)
                .round()
                .clamp(0.0, f64::from((1u32 << bits) - 1)) as u16
        };
        for t in &mut s.thresholds {
            t[0] = field(t[0], k, 16);
            t[1] = field(t[1], k, 12);
        }
        s.positive_strength = field(s.positive_strength, strength, 12);
        s.negative_strength = field(s.negative_strength, strength, 12);
        s.positive_pre_limit = field(s.positive_pre_limit, limit, 16);
        s.positive_limit = field(s.positive_limit, limit, 16);
        s.negative_pre_limit = field(s.negative_pre_limit, limit, 16);
        s.negative_limit = field(s.negative_limit, limit, 16);
        self.cfg.sharpen = s;
        self.cfg.sh_fc_combine = shfc;
    }

    fn enable_bayer(&mut self, bits: u32, on: bool) {
        if on {
            self.cfg.global.bayer_enables |= bits;
        } else {
            self.cfg.global.bayer_enables &= !bits;
        }
    }

    /// Gamma from a curve of `(x, y)` points on 16-bit scales.
    pub fn set_gamma_curve(&mut self, points: &[(u32, u32)]) {
        self.cfg.gamma = defaults::gamma_from_curve(points);
    }

    /// The YCbCr encoding used inside the pipeline (for sharpening/false colour).
    pub fn set_ycbcr(&mut self, name: &str) -> bool {
        let Some(e) = defaults::encoding(name) else {
            return false;
        };
        self.cfg.ycbcr = e.ycbcr;
        self.cfg.ycbcr_inverse = e.inverse;
        true
    }

    /// Output `i` format; width, height and strides left at zero are computed.
    pub fn set_output_format(&mut self, i: usize, f: BeOutputFormatConfig) {
        self.cfg.output_format[i] = f;
    }

    /// Output `i` crop of the input (zero width = no crop).
    pub fn set_crop(&mut self, i: usize, c: BeCropConfig) {
        self.crop[i] = c;
    }

    /// Output `i` colour space conversion.
    pub fn set_csc(&mut self, i: usize, c: BeCcmConfig) {
        self.cfg.csc[i] = c;
    }

    /// Output `i` size, choosing downscaler and resampler automatically ("smart resize").
    pub fn set_smart_resize(&mut self, i: usize, width: u16, height: u16) {
        self.smart_resize[i] = (width, height);
    }

    /// Output `i` downscaler target size.
    pub fn set_downscale(&mut self, i: usize, e: BeDownscaleExtra) {
        self.downscale_extra[i] = e;
    }

    /// Output `i` resampler target size and phases.
    pub fn set_resample(&mut self, i: usize, e: BeResampleExtra) {
        self.resample_extra[i] = e;
    }

    /// Limits the tile width (for testing tiling; at most [`MAX_TILE_WIDTH`]).
    pub fn set_max_tile_width(&mut self, w: u16) {
        self.max_tile_width = w.clamp(BE_MIN_TILE_WIDTH as u16, MAX_TILE_WIDTH);
    }

    /// A fixed Bayer-to-output pipeline: black level, white balance gains, demosaic, CCM
    /// (identity when `None`), the default gamma, false colour suppression, default
    /// sharpening, and output 0 in `format` (a [`crate::format::formats`] value) at the
    /// input size, with a full-range BT.601 ("jpeg") conversion for YUV outputs.
    pub fn simple_bayer(
        input: ImageFormatConfig,
        bayer: BayerOrder,
        black_level: u16,
        wb: (f64, f64, f64),
        ccm: Option<[f64; 9]>,
        format: u32,
    ) -> Self {
        let mut be = Self::new();
        be.set_input_format(input);
        be.set_black_level(black_level);
        be.set_wb_gains(wb.0, wb.1, wb.2);
        be.set_ccm(ccm.unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]));
        let mut bayer_en = bayer_enable::INPUT | bayer_enable::BLC | bayer_enable::WBG;
        let mut rgb_en = rgb_enable::CCM
            | rgb_enable::YCBCR
            | rgb_enable::YCBCR_INVERSE
            | rgb_enable::SHARPEN
            | rgb_enable::GAMMA
            | rgb_enable::OUTPUT0;
        if bayer != BayerOrder::Greyscale {
            bayer_en |= bayer_enable::DEMOSAIC;
            rgb_en |= rgb_enable::FALSE_COLOUR;
        }
        if image_format::is_three_channel(format) && format & image_format::SAMPLING_MASK != 0
            || format & image_format::PLANARITY_MASK != 0
        {
            be.set_csc(0, defaults::encoding("jpeg").expect("jpeg").ycbcr);
            rgb_en |= rgb_enable::CSC0;
        }
        be.set_global(bayer_en, rgb_en, bayer);
        be.set_output_format(
            0,
            BeOutputFormatConfig {
                image: ImageFormatConfig {
                    format,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        be
    }
}

#[cfg(test)]
mod tests;
