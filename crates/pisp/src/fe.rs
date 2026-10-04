//! Front end configuration builder.
//!
//! The front end takes the CSI-2 stream (`input.streaming = 1`, always 16 bits per sample as
//! the receiver unpacks it), applies black level / DPC / decompanding to the image path, writes
//! up to two raw outputs (each with crop, Bayer downscale and optional compression), and
//! computes statistics (AWB zones, AGC zones/histogram/row sums, CDAF, floating regions) on a
//! separate path with its own black level (`blc`), lens shading and RGB-to-Y weights.
//!
//! The finalisation rules (`prepare`) follow libpisp `src/libpisp/frontend/frontend.cpp`
//! (BSD-2-Clause, Copyright (C) 2021 - 2023, Raspberry Pi Ltd), ported to Rust. One libpisp
//! slip is not carried over: `fixOutputSize` writes the crop height into the width.

use alloc::{format, string::String};

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

use crate::format::compute_stride_align;
use crate::uapi::*;

/// Front end LSC interpolation precision (libpisp `FrontEnd::InterpPrecision`).
const LSC_INTERP_PRECISION: u32 = 6;
/// Front end LSC scale precision (libpisp `FrontEnd::ScalePrecision`).
const LSC_SCALE_PRECISION: u32 = 10;
/// Output stride alignment used by libcamera (`FrontEnd(true, variant)` default align 64).
pub const FE_OUTPUT_ALIGN: u32 = 64;

/// Unsigned 4.10 fixed point, clamped (1.0 = 1024).
pub fn gain_4_10(v: f64) -> u16 {
    (v * 1024.0).round().clamp(0.0, 16383.0) as u16
}

/// Builds `pisp_fe_config` buffers. Setters mark their block dirty; [`FrontEnd::prepare`]
/// finalises sizes and returns the buffer contents for one frame.
#[derive(Clone, Debug)]
pub struct FrontEnd {
    cfg: FeConfig,
}

impl FrontEnd {
    /// A streaming front end for a `width`x`height` 16-bit Bayer (or mono) input, with the
    /// output AXI settings libpisp uses for streaming.
    pub fn new(width: u16, height: u16, bayer: BayerOrder) -> Self {
        let cfg = FeConfig {
            output_axi: FeOutputAxiConfig {
                maxlen_flags: 0xaf,
                cache_prot: 0x32,
                qos: 0x8410,
                thresh: 0x0140,
                throttle: 0x4100,
            },
            dirty_flags_extra: fe_dirty::OUTPUT_AXI,
            ..FeConfig::default()
        };
        let mut fe = Self { cfg };
        fe.set_input(FeInputConfig {
            streaming: 1,
            format: ImageFormatConfig {
                width,
                height,
                format: image_format::BPS_16,
                ..Default::default()
            },
            ..Default::default()
        });
        fe.set_enables(fe_enable::INPUT, bayer);
        fe
    }

    /// The configuration as it stands.
    pub fn config(&self) -> &FeConfig {
        &self.cfg
    }

    /// Current enables.
    pub fn enables(&self) -> u32 {
        self.cfg.global.enables
    }

    /// Replaces the enables and Bayer order; newly enabled blocks become dirty.
    pub fn set_enables(&mut self, enables: u32, bayer: BayerOrder) {
        self.cfg.dirty_flags |= enables & !self.cfg.global.enables;
        self.cfg.global.enables = enables;
        self.cfg.global.bayer_order = bayer as u8;
        self.cfg.dirty_flags_extra |= fe_dirty::GLOBAL;
    }

    /// Enables (`on`) or disables blocks, keeping the Bayer order.
    pub fn enable(&mut self, blocks: u32, on: bool) {
        let enables = if on {
            self.cfg.global.enables | blocks
        } else {
            self.cfg.global.enables & !blocks
        };
        self.cfg.dirty_flags |= enables & !self.cfg.global.enables;
        self.cfg.global.enables = enables;
        self.cfg.dirty_flags_extra |= fe_dirty::GLOBAL;
    }

    fn dirty(&mut self, flag: u32) {
        self.cfg.dirty_flags |= flag;
    }

    /// Input format (streaming).
    pub fn set_input(&mut self, input: FeInputConfig) {
        self.cfg.input = input;
        self.dirty(fe_enable::INPUT);
    }

    /// Input decompression.
    pub fn set_decompress(&mut self, d: DecompressConfig) {
        self.cfg.decompress = d;
        self.dirty(fe_enable::DECOMPRESS);
    }

    /// Decompanding LUT.
    pub fn set_decompand(&mut self, mut d: FeDecompandConfig) {
        d.pad = 0;
        self.cfg.decompand = d;
        self.dirty(fe_enable::DECOMPAND);
    }

    /// Defective pixel correction.
    pub fn set_dpc(&mut self, d: FeDpcConfig) {
        self.cfg.dpc = d;
        self.dirty(fe_enable::DPC);
    }

    /// Black level on the image path (per channel, 16-bit scale).
    pub fn set_bla(&mut self, b: BlaConfig) {
        self.cfg.bla = b;
        self.dirty(fe_enable::BLA);
    }

    /// Black level on the statistics path.
    pub fn set_blc(&mut self, b: BlaConfig) {
        self.cfg.blc = b;
        self.dirty(fe_enable::BLC);
    }

    /// Statistics crop.
    pub fn set_stats_crop(&mut self, c: FeCropConfig) {
        self.cfg.stats_crop = c;
        self.dirty(fe_enable::STATS_CROP);
    }

    /// RGB to Y weights (and white balance gains folded in) for AGC statistics.
    pub fn set_rgby(&mut self, r: FeRgbyConfig) {
        self.cfg.rgby = r;
        self.dirty(fe_enable::RGBY);
    }

    /// Radial lens shading on the statistics path (zero centre/scale are filled in).
    pub fn set_lsc(&mut self, l: FeLscConfig) {
        self.cfg.lsc = l;
        self.dirty(fe_enable::LSC);
    }

    /// AGC statistics (zero sizes are filled in to cover the image).
    pub fn set_agc_stats(&mut self, a: FeAgcStatsConfig) {
        self.cfg.agc_stats = a;
        self.dirty(fe_enable::AGC_STATS);
    }

    /// AWB statistics (zero sizes are filled in to cover the image).
    pub fn set_awb_stats(&mut self, a: FeAwbStatsConfig) {
        self.cfg.awb_stats = a;
        self.dirty(fe_enable::AWB_STATS);
    }

    /// CDAF statistics (zero sizes are filled in to cover the image).
    pub fn set_cdaf_stats(&mut self, c: FeCdafStatsConfig) {
        self.cfg.cdaf_stats = c;
        self.dirty(fe_enable::CDAF_STATS);
    }

    /// Floating statistics regions.
    pub fn set_floating_stats(&mut self, f: FeFloatingStatsConfig) {
        self.cfg.floating_stats = f;
        self.cfg.dirty_flags_extra |= fe_dirty::FLOATING;
    }

    /// Output `i` crop.
    pub fn set_crop(&mut self, i: usize, c: FeCropConfig) {
        self.cfg.ch[i].crop = c;
        self.dirty(fe_enable::crop(i));
    }

    /// Output `i` downscale by `xout/xin` x `yout/yin` (output size is computed).
    pub fn set_downscale(&mut self, i: usize, d: FeDownscaleConfig) {
        self.cfg.ch[i].downscale = d;
        self.dirty(fe_enable::downscale(i));
    }

    /// Output `i` compression.
    pub fn set_compress(&mut self, i: usize, c: CompressConfig) {
        self.cfg.ch[i].compress = c;
        self.dirty(fe_enable::compress(i));
    }

    /// Output `i` format (size and stride are computed by `prepare` when zero).
    pub fn set_output_format(&mut self, i: usize, format: ImageFormatConfig) {
        self.cfg.ch[i].output.format = format;
        self.dirty(fe_enable::output(i));
    }

    /// The statistics set-up libcamera's PiSP IPA uses at start: statistics crop to the whole
    /// input, AWB over the frame with pixels above 98% excluded, uniform AGC weights,
    /// Gr/Gb CDAF, the first floating region over the whole frame, and RGB-to-Y weights for
    /// white balance gains `wb_r`/`wb_b`. `black_level` (16-bit scale) goes to both the image
    /// path (BLA) and the statistics path (BLC).
    pub fn default_stats(&mut self, black_level: u16, wb_r: f64, wb_b: f64) {
        let (w, h) = (self.cfg.input.format.width, self.cfg.input.format.height);
        self.set_stats_crop(FeCropConfig {
            offset_x: 0,
            offset_y: 0,
            width: w,
            height: h,
        });
        let hi = (65535.0 * 0.98) as u16;
        self.set_awb_stats(FeAwbStatsConfig {
            r_hi: hi,
            g_hi: hi,
            b_hi: hi,
            ..Default::default()
        });
        self.set_cdaf_stats(FeCdafStatsConfig {
            mode: (1 << 4) + (1 << 2) + 1,
            ..Default::default()
        });
        let mut agc: FeAgcStatsConfig = bytemuck::Zeroable::zeroed();
        agc.weights = [0x11; AGC_STATS_NUM_ZONES / 2];
        self.set_agc_stats(agc);
        let mut floating = FeFloatingStatsConfig::default();
        floating.regions[0].size_x = w;
        floating.regions[0].size_y = h;
        self.set_floating_stats(floating);
        self.set_rgby(FeRgbyConfig {
            gain_r: gain_4_10(wb_r * 0.299),
            gain_g: gain_4_10(0.587),
            gain_b: gain_4_10(wb_b * 0.114),
            ..Default::default()
        });
        let bl = BlaConfig {
            black_level_r: black_level,
            black_level_gr: black_level,
            black_level_gb: black_level,
            black_level_b: black_level,
            output_black_level: black_level,
            pad: [0; 2],
        };
        self.set_bla(bl);
        self.set_blc(BlaConfig {
            output_black_level: 0,
            ..bl
        });
        self.enable(
            fe_enable::STATS_CROP
                | fe_enable::AWB_STATS
                | fe_enable::AGC_STATS
                | fe_enable::CDAF_STATS
                | fe_enable::RGBY
                | fe_enable::BLA
                | fe_enable::BLC,
            true,
        );
    }

    /// Finalises the dirty, enabled blocks and returns the buffer for one frame; dirty
    /// flags are cleared for the next frame.
    pub fn prepare(&mut self) -> Result<FeConfig, String> {
        let c = &mut self.cfg;
        let dirty = c.dirty_flags & c.global.enables;
        let (mut w, mut h) = (c.input.format.width, c.input.format.height);
        if c.global.enables & fe_enable::STATS_CROP != 0 {
            w = c.stats_crop.width;
            h = c.stats_crop.height;
        }
        if dirty & fe_enable::LSC != 0 {
            finalise_lsc(&mut c.lsc, w, h)?;
        }
        if dirty & fe_enable::AGC_STATS != 0 {
            finalise_agc(&mut c.agc_stats, w, h);
        }
        if dirty & fe_enable::AWB_STATS != 0 {
            finalise_awb(&mut c.awb_stats, w, h);
        }
        if dirty & fe_enable::CDAF_STATS != 0 {
            finalise_cdaf(&mut c.cdaf_stats, w, h);
        }
        let (w, h) = (c.input.format.width, c.input.format.height);
        for i in 0..FE_NUM_OUTPUTS {
            let enables = c.global.enables;
            if dirty & fe_enable::downscale(i) != 0 {
                let (cw, ch) = if enables & fe_enable::crop(i) != 0 {
                    (c.ch[i].crop.width, c.ch[i].crop.height)
                } else {
                    (w, h)
                };
                let d = &mut c.ch[i].downscale;
                if d.xin == 0 || d.yin == 0 {
                    return Err(format!("output {i}: downscale xin/yin is zero"));
                }
                d.output_width =
                    ((u32::from(cw >> 1) * u32::from(d.xout) / u32::from(d.xin)) * 2) as u16;
                d.output_height =
                    ((u32::from(ch >> 1) * u32::from(d.yout) / u32::from(d.yin)) * 2) as u16;
            }
            if dirty & (fe_enable::output(i) | fe_enable::compress(i)) != 0 {
                let fmt = c.ch[i].output.format.format;
                let compressing = enables & fe_enable::compress(i) != 0;
                if image_format::is_compressed(fmt) != compressing {
                    return Err(format!("output {i}: compression and format disagree"));
                }
                if compressing && fmt & image_format::BPS_MASK != image_format::BPS_8 {
                    return Err(format!("output {i}: compressed output must be 8 bit"));
                }
            }
            if dirty & fe_enable::output(i) != 0 {
                let (mut ow, mut oh) = (w, h);
                if enables & fe_enable::crop(i) != 0 {
                    (ow, oh) = (c.ch[i].crop.width, c.ch[i].crop.height);
                }
                if enables & fe_enable::downscale(i) != 0 {
                    let d = &c.ch[i].downscale;
                    (ow, oh) = (d.output_width, d.output_height);
                }
                let img = &mut c.ch[i].output.format;
                img.width = ow;
                img.height = oh;
                if img.stride == 0 {
                    compute_stride_align(img, FE_OUTPUT_ALIGN);
                }
            }
        }
        let mut out = *c;
        if out.global.enables & fe_enable::DECIMATE != 0 {
            decimate(&mut out);
        }
        c.dirty_flags = 0;
        c.dirty_flags_extra = 0;
        Ok(out)
    }
}

fn finalise_lsc(lsc: &mut FeLscConfig, w: u16, h: u16) -> Result<(), String> {
    if lsc.centre_x == 0 {
        lsc.centre_x = w / 2;
    }
    if lsc.centre_y == 0 {
        lsc.centre_y = h / 2;
    }
    if lsc.scale == 0 {
        let dx = u32::from((w - lsc.centre_x).max(lsc.centre_x));
        let dy = u32::from((h - lsc.centre_y).max(lsc.centre_y));
        let mut r2 = dx * dx + dy * dy;
        if r2 >= 1 << 31 {
            return Err("LSC radius too large".into());
        }
        let span = ((FE_LSC_LUT_SIZE as u32) - 1) << LSC_INTERP_PRECISION;
        lsc.shift = 0;
        while r2 >= 2 * span {
            r2 >>= 1;
            lsc.shift += 1;
        }
        let scale = ((1u32 << LSC_SCALE_PRECISION) * span - 1) / r2;
        lsc.scale = scale.min((1 << LSC_SCALE_PRECISION) - 1) as u16;
    }
    Ok(())
}

fn grid(total: u16, offset: u16, cells: usize) -> u16 {
    (((i32::from(total) - 2 * i32::from(offset)) / cells as i32) & !1).max(2) as u16
}

fn finalise_agc(a: &mut FeAgcStatsConfig, w: u16, h: u16) {
    if a.size_x == 0 {
        a.size_x = grid(w, a.offset_x, AGC_STATS_SIZE);
    }
    if a.size_y == 0 {
        a.size_y = grid(h, a.offset_y, AGC_STATS_SIZE);
    }
    if a.row_size_x == 0 {
        a.row_size_x = grid(w, a.row_offset_x, 1);
    }
    if a.row_size_y == 0 {
        a.row_size_y = grid(h, a.row_offset_y, AGC_STATS_NUM_ROW_SUMS);
    }
}

fn finalise_awb(a: &mut FeAwbStatsConfig, w: u16, h: u16) {
    let cell = |total: u16, off: u16| {
        let n = (i32::from(total) - 2 * i32::from(off) + AWB_STATS_SIZE as i32)
            / (2 * AWB_STATS_SIZE as i32);
        (2 * n.max(1)) as u16
    };
    if a.size_x == 0 {
        a.size_x = cell(w, a.offset_x);
    }
    if a.size_y == 0 {
        a.size_y = cell(h, a.offset_y);
    }
}

fn finalise_cdaf(c: &mut FeCdafStatsConfig, w: u16, h: u16) {
    if c.size_x == 0 {
        c.size_x = grid(w, c.offset_x, CDAF_STATS_SIZE);
    }
    if c.size_y == 0 {
        c.size_y = grid(h, c.offset_y, CDAF_STATS_SIZE);
    }
}

/// Halve and round to even (statistics decimation).
fn half_even(v: &mut u16) {
    *v = ((*v + 2) & !3) >> 1;
}

fn decimate(c: &mut FeConfig) {
    let e = c.global.enables;
    if e & fe_enable::LSC != 0 {
        half_even(&mut c.lsc.centre_x);
        half_even(&mut c.lsc.centre_y);
    }
    if e & fe_enable::CDAF_STATS != 0 {
        let d = &mut c.cdaf_stats;
        for v in [
            &mut d.offset_x,
            &mut d.offset_y,
            &mut d.size_x,
            &mut d.size_y,
            &mut d.skip_x,
            &mut d.skip_y,
        ] {
            half_even(v);
        }
    }
    if e & fe_enable::AWB_STATS != 0 {
        let d = &mut c.awb_stats;
        for v in [
            &mut d.offset_x,
            &mut d.offset_y,
            &mut d.size_x,
            &mut d.size_y,
        ] {
            half_even(v);
        }
    }
    if e & fe_enable::AGC_STATS != 0 {
        let d = &mut c.agc_stats;
        for v in [
            &mut d.offset_x,
            &mut d.offset_y,
            &mut d.size_x,
            &mut d.size_y,
            &mut d.row_offset_x,
            &mut d.row_offset_y,
            &mut d.row_size_x,
            &mut d.row_size_y,
        ] {
            half_even(v);
        }
    }
    for r in &mut c.floating_stats.regions {
        for v in [
            &mut r.offset_x,
            &mut r.offset_y,
            &mut r.size_x,
            &mut r.size_y,
        ] {
            half_even(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stats_cover_the_frame() {
        let mut fe = FrontEnd::new(1280, 800, BayerOrder::Bggr);
        fe.default_stats(4096, 1.5, 1.5);
        fe.set_output_format(
            0,
            ImageFormatConfig {
                format: image_format::BPS_16,
                ..Default::default()
            },
        );
        fe.enable(fe_enable::OUTPUT0, true);
        let c = fe.prepare().unwrap();
        assert_eq!((c.awb_stats.size_x, c.awb_stats.size_y), (40, 26));
        assert_eq!((c.agc_stats.size_x, c.agc_stats.size_y), (80, 50));
        assert_eq!((c.agc_stats.row_size_x, c.agc_stats.row_size_y), (1280, 2));
        assert_eq!((c.cdaf_stats.size_x, c.cdaf_stats.size_y), (160, 100));
        let out = c.ch[0].output.format;
        assert_eq!((out.width, out.height, out.stride), (1280, 800, 2560));
        assert_eq!(c.input.streaming, 1);
        assert_eq!(c.global.bayer_order, 2);
        assert_ne!(c.dirty_flags & fe_enable::AWB_STATS, 0);
        assert_eq!(fe.config().dirty_flags, 0);
    }

    #[test]
    fn downscale_and_crop_sizes() {
        let mut fe = FrontEnd::new(1280, 800, BayerOrder::Rggb);
        fe.set_crop(
            1,
            FeCropConfig {
                offset_x: 0,
                offset_y: 0,
                width: 640,
                height: 400,
            },
        );
        fe.set_downscale(
            1,
            FeDownscaleConfig {
                xin: 2,
                xout: 1,
                yin: 2,
                yout: 1,
                flags: FE_DOWNSCALE_BAYER,
                ..Default::default()
            },
        );
        fe.set_output_format(1, ImageFormatConfig::default());
        fe.enable(
            fe_enable::CROP1 | fe_enable::DOWNSCALE1 | fe_enable::OUTPUT1,
            true,
        );
        let c = fe.prepare().unwrap();
        let out = c.ch[1].output.format;
        assert_eq!((out.width, out.height), (320, 200));
    }

    #[test]
    fn compression_must_match_format() {
        let mut fe = FrontEnd::new(64, 64, BayerOrder::Rggb);
        fe.set_output_format(0, ImageFormatConfig::default());
        fe.enable(fe_enable::OUTPUT0 | fe_enable::COMPRESS0, true);
        assert!(fe.prepare().is_err());
    }

    #[test]
    fn decimation_halves_grids() {
        let mut fe = FrontEnd::new(1280, 800, BayerOrder::Rggb);
        fe.default_stats(0, 1.0, 1.0);
        fe.enable(fe_enable::DECIMATE, true);
        let c = fe.prepare().unwrap();
        assert_eq!(c.awb_stats.size_x, 20);
        assert_eq!(c.floating_stats.regions[0].size_x, 640);
    }
}
