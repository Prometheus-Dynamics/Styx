//! The statistics the ISP measured, as the 3A's input: [`statistics`] turns Lemnos's decoded
//! statistics ([`Raw`]) into [`styx_aaa::Statistics`].
//!
//! The AE block gives a 5x5 grid of 8-bit luma means: the 3A's luma grid. The AWB block gives one
//! white-pixel mean per channel and the number of pixels it averaged: a 1x1 colour grid. The
//! histogram is 16 luma bins. The raw blocks measure before white balance and lens shading (the
//! headers do not say; this is the assumption, to be confirmed on a board).

use styx_aaa::{ColourZone, LumaZone, Statistics};
use styx_lemnos::k230isp::regs::{AE_GRID, HIST_BIN_FRAC_BITS};
use styx_lemnos::k230isp::stats::Raw;

/// Full scale of an AE or AWB mean (8 bits).
pub const MEAN_FULL: f64 = 255.0;

/// `raw` as [`Statistics`], written into `out` (its buffers reused). `pixels_per_block` is the
/// pixel count of one AE block (the window [`crate::Params::measure`] set): each block's mean is
/// that many pixels at the mean's value.
pub fn statistics(raw: &Raw, pixels_per_block: u32, out: &mut Statistics) {
    let n = f64::from(pixels_per_block.max(1));
    let side = AE_GRID;
    out.luma = Some(styx_aaa::ZoneGrid {
        width: side,
        height: side,
        zones: raw
            .ae
            .iter()
            .map(|&m| LumaZone {
                y: f64::from(m) / MEAN_FULL * n,
                counted: pixels_per_block,
            })
            .collect(),
    });

    let white = raw.awb_white;
    let w = f64::from(white);
    let [red, green, blue] = raw.awb_mean.map(|m| f64::from(m) / MEAN_FULL);
    out.colour.width = 1;
    out.colour.height = 1;
    out.colour.zones.clear();
    out.colour.zones.push(ColourZone {
        r: red * w,
        g: green * w,
        b: blue * w,
        counted: white,
    });

    out.histogram
        .set_bins(raw.hist.iter().map(|&b| u64::from(b >> HIST_BIN_FRAC_BITS)));
    out.focus = None;
    out.pdaf = None;
    out.before_wb = true;
    out.before_lsc = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn means_become_sums_over_their_pixels() {
        let mut raw = Raw::default();
        raw.ae[0] = 255;
        raw.ae[24] = 0;
        raw.awb_mean = [51, 102, 153];
        raw.awb_white = 1000;
        raw.hist[0] = 16 * 5;
        let mut out = Statistics::default();
        statistics(&raw, 100, &mut out);
        let luma = out.luma.as_ref().expect("a luma grid");
        assert_eq!((luma.width, luma.height), (5, 5));
        assert_eq!(luma.zones[0].y, 100.0);
        assert_eq!(luma.zones[0].counted, 100);
        assert_eq!(luma.zones[24].y, 0.0);
        let c = out.colour.zones[0];
        assert_eq!(c.counted, 1000);
        assert!((c.r - 200.0).abs() < 1e-9);
        assert!((c.g - 400.0).abs() < 1e-9);
        assert!((c.b - 600.0).abs() < 1e-9);
        assert_eq!(out.histogram.bins()[0], 5);
        assert!(out.before_wb && out.before_lsc);
    }

    #[test]
    fn a_zero_block_counts_one_pixel() {
        let mut out = Statistics::default();
        statistics(&Raw::default(), 0, &mut out);
        assert_eq!(out.luma.expect("grid").zones[0].counted, 0);
    }
}
