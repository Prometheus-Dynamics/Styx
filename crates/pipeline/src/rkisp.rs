//! ISP settings as the Rockchip ISP30's parameter buffer ([`params`]), and its statistics as
//! the algorithms' input ([`statistics`]).
//!
//! The ISP30 blocks these settings drive: black level (BLS), white balance and digital gain
//! (AWB gain), the colour matrix (CCM) and the output gamma (GOC). Lens shading, denoise and
//! sharpening stay at the driver's defaults for now.

use styx_algo::Statistics;
#[allow(unused_imports)]
use styx_core::math::Float as _;
use styx_rkisp::Params;
use styx_softisp::ToneCurve;

use crate::isp::IspSettings;

/// The 12-bit scale the ISP30's black levels and colour matrix offsets are on.
const LEVEL12: f64 = 4096.0;

/// The parameter buffer for frame `frame` with `settings`, measuring the statistics over a
/// `width` x `height` raw frame.
pub fn params(settings: &IspSettings, frame: u32, width: u32, height: u32) -> Params {
    let level = |v: f64| (v * LEVEL12).round().clamp(0.0, LEVEL12 - 1.0) as u16;
    let gain = settings.digital_gain;
    let wb = settings.wb.map(|g| (g * gain) as f32);
    let m = settings.ccm.map(|c| c as f32);
    let ccm = [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]];
    let curve = settings.gamma.clone();
    Params::new()
        .frame(frame)
        .measure(dim(width), dim(height))
        .black_level(settings.black_cfa().map(level))
        .white_balance(wb)
        .colour_matrix(ccm, [0.0; 3])
        .gamma(move |x| match &curve {
            Some(p) if p.points().len() >= 2 => p.eval_clamped(f64::from(x)) as f32,
            _ => ToneCurve::Srgb.eval(x),
        })
}

fn dim(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

/// A statistics buffer of a `width` x `height` raw frame (measured by [`params`]) into `out`;
/// `false` when the buffer is short.
pub fn statistics(buf: &[u8], width: u32, height: u32, out: &mut Statistics) -> bool {
    let zones = styx_rkisp::AE_ZONES;
    let per_zone = (width / zones) * (height / zones);
    styx_rkisp::statistics(buf, per_zone, out).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> IspSettings {
        IspSettings {
            wb: [2.0, 1.0, 1.5],
            digital_gain: 1.25,
            ..IspSettings::neutral(64.0 / 1024.0)
        }
    }

    #[test]
    fn the_settings_fill_their_blocks() {
        let p = params(&settings(), 7, 1280, 800);
        let c = p.config();
        assert_eq!({ c.frame_id }, 7);
        let bls = c.others.bls_cfg;
        assert_eq!({ bls.fixed_val.r }, 256, "64 of 10 bits is 256 of 12");
        let awb = c.others.awb_gain_cfg;
        assert_eq!({ awb.gain0_red }, 640, "2.0 x 1.25 x 256");
        assert_eq!({ awb.gain0_green_r }, 320);
        assert_eq!({ awb.gain0_blue }, 480);
        let ccm = c.others.ccm_cfg;
        assert_eq!({ ccm.coeff0_r }, 128, "the identity matrix's 1.0");
        assert_eq!({ ccm.coeff1_r }, 0);
        let rawae = c.meas.rawae3;
        assert_eq!(({ rawae.win.h_size }, { rawae.win.v_size }), (1280, 800));
    }

    #[test]
    fn no_curve_is_the_srgb_curve() {
        let p = params(&settings(), 0, 640, 400);
        let g = { p.config().others.gammaout_cfg.gamma_y };
        assert_eq!(g[0], 0);
        assert_eq!(*g.last().unwrap(), 4095);
        let mid = g[g.len() / 2];
        assert!((3000..3300).contains(&mid), "sRGB lifts the middle: {mid}");
    }
}
