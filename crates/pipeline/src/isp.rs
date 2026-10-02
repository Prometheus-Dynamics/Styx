//! The ISP part of the algorithms' output, and how it maps onto each ISP.

use serde::{Deserialize, Serialize};
use styx_algo::{IDENTITY, LensShading, Matrix3, Params, Pwl};
use styx_pisp::be::BackEnd;
use styx_pisp::fe::{FrontEnd, gain_4_10};
use styx_pisp::uapi::{
    BayerOrder, BeLscConfig, BeLscExtra, BeOutputFormatConfig, BlaConfig, FeRgbyConfig,
    ImageFormatConfig, image_format, rgb_enable,
};
use styx_softisp as soft;

/// What the ISP applies to a frame: everything is normalised (full scale 1.0) and hardware
/// independent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IspSettings {
    /// The frame whose statistics produced these settings.
    pub from_frame: u64,
    /// Black level to subtract (the smallest of the per-channel levels).
    pub black_level: f64,
    /// White balance gains, green 1.
    pub wb: [f64; 3],
    /// Digital gain on top of the white balance (exposure the sensor could not provide, and
    /// the difference while a new exposure is still on its way).
    pub digital_gain: f64,
    /// Colour correction matrix (row-major, camera RGB to output RGB after white balance).
    pub ccm: Matrix3,
    /// Tone curve on `[0, 1]`; `None` is the sRGB curve.
    pub gamma: Option<Pwl>,
    /// Lens shading gains, if the tuning calibrates them.
    pub lens_shading: Option<LensShading>,
}

impl IspSettings {
    /// Neutral settings: black level only, unity gains, identity matrix, sRGB tone.
    pub fn neutral(black_level: f64) -> Self {
        Self {
            from_frame: 0,
            black_level,
            wb: [1.0; 3],
            digital_gain: 1.0,
            ccm: IDENTITY,
            gamma: None,
            lens_shading: None,
        }
    }

    /// The settings the algorithms' `params` (from frame `frame`) ask for, with `digital_gain`
    /// for the frame being processed.
    pub fn from_params(p: &Params, frame: u64, digital_gain: f64) -> Self {
        let bl = p.black_level;
        let g = p.colour_gains[1].max(1e-6);
        Self {
            from_frame: frame,
            black_level: bl.r.min(bl.g).min(bl.b),
            wb: [p.colour_gains[0] / g, 1.0, p.colour_gains[2] / g],
            digital_gain: digital_gain * g,
            ccm: p.ccm,
            gamma: p.gamma.clone(),
            lens_shading: p.lens_shading.clone(),
        }
    }

    /// White balance times digital gain, per channel.
    pub fn channel_gains(&self) -> [f64; 3] {
        self.wb.map(|w| w * self.digital_gain)
    }

    /// Software ISP parameters for `bits`-bit samples, keeping `base`'s demosaic, YUV
    /// matrix and statistics settings.
    pub fn softisp(&self, bits: u8, base: &soft::IspParams) -> soft::IspParams {
        let full = f64::from((1u32 << bits) - 1);
        let level = (self.black_level * f64::from(1u32 << bits))
            .round()
            .clamp(0.0, full - 1.0);
        let m = self.ccm.map(|v| v.clamp(-3.99, 3.99) as f32);
        soft::IspParams {
            black_level: Some(soft::BlackLevel::uniform(level as u16)),
            white_balance: Some(soft::WhiteBalance {
                r: self.wb[0] as f32,
                g: self.wb[1] as f32,
                b: self.wb[2] as f32,
            }),
            digital_gain: self.digital_gain as f32,
            lens_shading: self.lens_shading.as_ref().and_then(soft_lens_shading),
            ccm: Some(soft::ColorMatrix {
                m: [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]],
            }),
            tone: Some(match &self.gamma {
                Some(p) => soft::ToneCurve::Points {
                    points: p
                        .points()
                        .iter()
                        .map(|&(x, y)| [x as f32, y as f32])
                        .collect(),
                },
                None => soft::ToneCurve::Srgb,
            }),
            ..base.clone()
        }
    }

    /// The back end blocks these settings drive: black level (BLC), lens shading (LSC, the
    /// tables resampled to the back end's 33x33 vertices over the input), white balance and
    /// digital gain (WBG), CCM and gamma.
    pub fn apply_be(&self, be: &mut BackEnd) {
        be.set_black_level(level16(self.black_level));
        if let Some(ls) = &self.lens_shading
            && let Some(cfg) = be_lens_shading(ls)
        {
            be.set_lsc(cfg, BeLscExtra::default());
        }
        let g = self.channel_gains();
        be.set_wb_gains(g[0], g[1], g[2]);
        be.set_ccm(self.ccm);
        be.set_gamma_curve(&gamma_points(self.gamma.as_ref()));
    }

    /// The front end blocks that follow the algorithms: black levels (image path BLA, keeping
    /// the black level in the raw output for the back end; statistics path BLC) and the
    /// RGB-to-Y weights of the AGC statistics (BT.601 times the white balance gains, as the
    /// Raspberry Pi IPA does).
    pub fn apply_fe(&self, fe: &mut FrontEnd) {
        let level = level16(self.black_level);
        let bl = BlaConfig {
            black_level_r: level,
            black_level_gr: level,
            black_level_gb: level,
            black_level_b: level,
            output_black_level: level,
            pad: [0; 2],
        };
        if fe.config().bla != bl {
            fe.set_bla(bl);
            fe.set_blc(BlaConfig {
                output_black_level: 0,
                ..bl
            });
        }
        let rgby = FeRgbyConfig {
            gain_r: gain_4_10(self.wb[0] * 0.299),
            gain_g: gain_4_10(self.wb[1] * 0.587),
            gain_b: gain_4_10(self.wb[2] * 0.114),
            ..fe.config().rgby
        };
        if fe.config().rgby != rgby {
            fe.set_rgby(rgby);
        }
    }
}

/// The back end config every frame starts from: the fixed Bayer pipeline (black level, white
/// balance, demosaic, CCM, sharpening, false colour, gamma) and both outputs at their sizes,
/// with the full-range BT.601 conversion on YUV outputs.
pub fn be_template(
    input: ImageFormatConfig,
    order: BayerOrder,
    black_level: f64,
    outputs: [Option<ImageFormatConfig>; 2],
) -> crate::Result<BackEnd> {
    let out0 = outputs[0].ok_or_else(|| crate::PipelineError::Config("no output 0".into()))?;
    let mut be = BackEnd::simple_bayer(
        input,
        order,
        level16(black_level),
        (1.0, 1.0, 1.0),
        None,
        out0.format,
    );
    let yuv = |f: u32| f & (image_format::SAMPLING_MASK | image_format::PLANARITY_MASK) != 0;
    let jpeg = styx_pisp::be::defaults::encoding("jpeg").expect("jpeg encoding");
    for (i, o) in outputs.iter().enumerate() {
        let Some(o) = o else { continue };
        be.set_output_format(
            i,
            BeOutputFormatConfig {
                image: *o,
                ..Default::default()
            },
        );
        if (o.width, o.height) != (input.width, input.height) {
            be.set_smart_resize(i, o.width, o.height);
        }
        let mut rgb = be.config().global.rgb_enables | rgb_enable::output(i);
        if yuv(o.format) {
            be.set_csc(i, jpeg.ycbcr);
            rgb |= rgb_enable::csc(i);
        }
        let g = be.config().global;
        be.set_global(g.bayer_enables, rgb, order);
    }
    Ok(be)
}

/// A normalised level on the 16-bit scale the PiSP works in.
pub fn level16(v: f64) -> u16 {
    (v * 65536.0).round().clamp(0.0, 65535.0) as u16
}

/// A tone curve as `(x, y)` points on 16-bit scales; the sRGB curve when `None`.
pub fn gamma_points(curve: Option<&Pwl>) -> Vec<(u32, u32)> {
    let to16 = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u32;
    match curve {
        Some(p) if p.points().len() >= 2 => p
            .points()
            .iter()
            .map(|&(x, y)| (to16(x), to16(y)))
            .collect(),
        _ => {
            let srgb = soft::ToneCurve::Srgb;
            (0..=64)
                .map(|i| {
                    let x = (i as f64 / 64.0).powi(2);
                    (to16(x), to16(f64::from(srgb.eval(x as f32))))
                })
                .collect()
        }
    }
}

/// Lens shading tables as the back end's packed 33x33 vertex grid.
pub(crate) fn be_lens_shading(ls: &LensShading) -> Option<BeLscConfig> {
    let (w, h) = (ls.width as usize, ls.height as usize);
    if w == 0 || h == 0 || [&ls.r, &ls.g, &ls.b].iter().any(|t| t.len() != w * h) {
        return None;
    }
    let table = [&ls.r, &ls.g, &ls.b].map(|t| styx_pisp::be::lsc::resample_table(t, w, h));
    Some(styx_pisp::be::lsc::pack_lut(&table))
}

fn soft_lens_shading(ls: &LensShading) -> Option<soft::LensShading> {
    let n = (ls.width * ls.height) as usize;
    if ls.width < 2 || ls.height < 2 || [&ls.r, &ls.g, &ls.b].iter().any(|t| t.len() != n) {
        return None;
    }
    let f = |t: &[f64]| t.iter().map(|&v| v as f32).collect();
    Some(soft::LensShading {
        width: ls.width,
        height: ls.height,
        r: f(&ls.r),
        g: f(&ls.g),
        b: f(&ls.b),
    })
}

#[cfg(test)]
mod tests {
    use styx_algo::BlackLevels;
    use styx_pisp::uapi::BayerOrder;

    use super::*;

    fn params() -> Params {
        Params {
            colour_gains: [2.0, 1.25, 1.5],
            ccm: [1.5, -0.25, -0.25, -0.2, 1.4, -0.2, 0.0, -0.5, 1.5],
            black_level: BlackLevels {
                r: 0.0625,
                g: 0.0625,
                b: 0.07,
            },
            gamma: Some(Pwl::new(vec![(0.0, 0.0), (0.5, 0.75), (1.0, 1.0)]).unwrap()),
            ..Params::default()
        }
    }

    #[test]
    fn settings_normalise_green_into_the_digital_gain() {
        let s = IspSettings::from_params(&params(), 7, 2.0);
        assert_eq!(s.from_frame, 7);
        assert_eq!(s.wb, [1.6, 1.0, 1.2]);
        assert_eq!(s.digital_gain, 2.5);
        assert_eq!(s.channel_gains(), [4.0, 2.5, 3.0]);
        assert_eq!(s.black_level, 0.0625);
    }

    #[test]
    fn softisp_parameters() {
        let s = IspSettings::from_params(&params(), 0, 1.0);
        let p = s.softisp(10, &soft::IspParams::default());
        assert_eq!(p.black_level, Some(soft::BlackLevel::uniform(64)));
        assert_eq!(p.digital_gain, 1.25);
        assert_eq!(p.ccm.unwrap().m[0], [1.5, -0.25, -0.25]);
        let Some(soft::ToneCurve::Points { points }) = p.tone.clone() else {
            panic!("points");
        };
        assert_eq!(points[1], [0.5, 0.75]);
        // A frame goes through with these parameters.
        let format = soft::RawFormat::new(
            8,
            8,
            soft::CfaPattern::Bggr,
            soft::RawPacking::U16Le { bits: 10 },
        );
        let raw = vec![0u8; 8 * 8 * 2];
        let mut rgb = vec![0u8; 8 * 8 * 3];
        soft::process(
            format,
            &p,
            &raw,
            16,
            soft::Scale::Full,
            soft::OutputBuffers::Rgb24 {
                data: &mut rgb,
                stride: 24,
            },
        )
        .unwrap();
    }

    #[test]
    fn back_end_template_with_two_outputs_prepares() {
        use styx_pisp::format::{compute_stride_align, formats};
        let mut input = ImageFormatConfig {
            width: 1280,
            height: 800,
            format: formats::BAYER16,
            ..Default::default()
        };
        compute_stride_align(&mut input, 64);
        let out = |w: u16, h: u16, format: u32| {
            let mut f = ImageFormatConfig {
                width: w,
                height: h,
                format,
                ..Default::default()
            };
            compute_stride_align(&mut f, 64);
            f
        };
        let mut be = be_template(
            input,
            BayerOrder::Bggr,
            64.0 / 1024.0,
            [
                Some(out(1280, 800, formats::NV12)),
                Some(out(640, 400, formats::RGB888)),
            ],
        )
        .unwrap();
        let mut p = params();
        p.lens_shading = Some(LensShading {
            width: 16,
            height: 12,
            r: vec![1.5; 192],
            g: vec![1.25; 192],
            b: vec![1.0; 192],
        });
        IspSettings::from_params(&p, 0, 1.0).apply_be(&mut be);
        let g = be.config().global;
        assert!(g.bayer_enables & styx_pisp::uapi::bayer_enable::LSC != 0);
        for bit in [
            rgb_enable::output(0),
            rgb_enable::output(1),
            rgb_enable::csc(0),
        ] {
            assert!(g.rgb_enables & bit != 0, "{bit:#x}");
        }
        // RGB output 1 needs no colour space conversion; it is resampled to half size.
        assert_eq!(g.rgb_enables & rgb_enable::csc(1), 0);
        let cfg = be.prepare().unwrap();
        assert!(cfg.num_tiles >= 3, "{}", cfg.num_tiles);
        // The grid spans the input; each tile starts at its offset into it.
        let step = be.config().lsc.grid_step_x;
        assert_eq!(u32::from(step), (32 << 18) / 1280);
        let t1 = &cfg.tiles[1];
        assert_eq!(
            t1.lsc_grid_offset_x,
            u32::from(t1.input_offset_x) * u32::from(step)
        );
        assert!(be.config().global.rgb_enables & rgb_enable::resample(1) != 0);
        assert!(be_template(input, BayerOrder::Bggr, 0.0, [None, None]).is_err());
    }

    #[test]
    fn pisp_blocks() {
        let s = IspSettings::from_params(&params(), 0, 1.0);
        let mut be = BackEnd::new();
        s.apply_be(&mut be);
        let c = be.config();
        assert_eq!(c.blc.black_level_r, 4096);
        assert_eq!(
            (c.wbg.gain_r, c.wbg.gain_g, c.wbg.gain_b),
            (2048, 1280, 1536)
        );
        assert_eq!(c.ccm.coeffs[0], 1536);
        let mut fe = FrontEnd::new(64, 64, BayerOrder::Bggr);
        s.apply_fe(&mut fe);
        assert_eq!(fe.config().bla.output_black_level, 4096);
        assert_eq!(fe.config().blc.output_black_level, 0);
        assert_eq!(fe.config().rgby.gain_r, gain_4_10(1.6 * 0.299));
        let srgb = gamma_points(None);
        assert_eq!((srgb[0], srgb[64]), ((0, 0), (65535, 65535)));
        assert!(srgb[16].1 > srgb[16].0);
    }
}
