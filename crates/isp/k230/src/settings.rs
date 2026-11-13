//! The neutral settings ([`styx_settings::IspSettings`]) as the K230D's ISP writes: [`params`],
//! and the statistics the ISP measured for them as the 3A's input ([`statistics`]).
//!
//! The blocks these settings drive: black level (BLS), white balance and digital gain (AWB
//! gains), the colour matrix and the output gamma. Lens shading is not written (see the crate
//! docs). The frame path (the ISP's input and output buffers) is not here: it is the sensor
//! and receiver's, as Lemnos runs them.

use styx_aaa::Statistics;
use styx_lemnos::k230isp::regs::AE_GRID;
use styx_lemnos::k230isp::stats::Raw;
use styx_settings::IspSettings;
use styx_soft::{CfaPattern, ToneCurve};

use crate::params::Params;
use crate::stats;

/// The 12-bit scale the ISP's black levels are on (`BLS_MAX` is 4095).
const LEVEL12: f64 = 4096.0;

/// The register writes for one frame with `settings`, measuring the statistics over a
/// `width` x `height` raw frame of mosaic `cfa`, built into `p` (reset first: one value reused
/// frame to frame, no allocation once its list has grown).
#[must_use]
pub fn params(
    p: Params,
    settings: &IspSettings,
    cfa: CfaPattern,
    (width, height): (u32, u32),
) -> Params {
    let level = |v: f64| (v * LEVEL12).round().clamp(0.0, LEVEL12 - 1.0) as u16;
    // The ISP's levels go by mosaic position (top-left, top-right, bottom-left, bottom-right),
    // the settings' by channel.
    let channels = settings.black_cfa();
    let black = [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(x, y)| level(channels[cfa.cell_at(x, y)]));
    let gain = settings.digital_gain;
    let wb = settings.wb.map(|g| (g * gain) as f32);
    let m = settings.ccm.map(|c| c as f32);
    let ccm = [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]];
    let curve = &settings.gamma;
    p.reset()
        .measure(width, height)
        .black_level(black)
        .white_balance(wb)
        .colour_matrix(ccm, [0.0; 3])
        .gamma(|x| match curve {
            Some(c) if c.points().len() >= 2 => c.eval_clamped(f64::from(x)) as f32,
            _ => ToneCurve::Srgb.eval(x),
        })
}

/// The pixels in one AE block of a `width` x `height` frame, as [`params`] measures them
/// (`len / 5`, rounded down to even).
#[must_use]
pub fn pixels_per_block(width: u32, height: u32) -> u32 {
    let block = |len: u32| (len / AE_GRID) & !1;
    block(width) * block(height)
}

/// The ISP's statistics of a `width` x `height` raw frame (measured by [`params`]) into `out`.
pub fn statistics(raw: &Raw, width: u32, height: u32, out: &mut Statistics) {
    stats::statistics(raw, pixels_per_block(width, height), out);
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
    fn rebuilding_gives_the_same_writes() {
        let first = params(Params::new(), &settings(), CfaPattern::Rggb, (1280, 800));
        assert!(!first.writes().is_empty());
        let again = params(first.clone(), &settings(), CfaPattern::Rggb, (1280, 800));
        assert_eq!(again, first, "reset, then the same writes");
        assert_ne!(first.ctrl_bits(), 0, "the gamma turns its block on");
    }

    #[test]
    fn rebuilding_does_not_grow_the_list() {
        let p = params(Params::new(), &settings(), CfaPattern::Rggb, (1280, 800));
        let n = p.writes().len();
        let p = params(p, &settings(), CfaPattern::Rggb, (1280, 800));
        assert_eq!(p.writes().len(), n);
    }

    #[test]
    fn black_levels_follow_the_mosaic() {
        let s = IspSettings {
            black_channels: Some([0.25, 0.5, 0.5, 0.75]),
            ..settings()
        };
        let rggb = params(Params::new(), &s, CfaPattern::Rggb, (1280, 800));
        let bggr = params(Params::new(), &s, CfaPattern::Bggr, (1280, 800));
        let bls = |p: &Params| -> Vec<u32> {
            let r =
                styx_lemnos::k230isp::regs::BLS_A_FIXED..=styx_lemnos::k230isp::regs::BLS_D_FIXED;
            p.writes()
                .iter()
                .filter(|w| r.contains(&w.addr))
                .map(|w| w.value)
                .collect()
        };
        assert_eq!(bls(&rggb), [1024, 2048, 2048, 3072]);
        assert_eq!(bls(&bggr), [3072, 2048, 2048, 1024]);
    }

    #[test]
    fn blocks_are_a_fifth_rounded_down_to_even() {
        assert_eq!(pixels_per_block(1280, 800), 256 * 160);
        assert_eq!(pixels_per_block(1282, 806), 256 * 160);
        assert_eq!(pixels_per_block(1290, 810), 258 * 162);
    }
}
