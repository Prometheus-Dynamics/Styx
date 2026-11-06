//! The statistics the ISP measured, read from its registers ([`read_raw`], Lemnos's) and turned
//! into Styx's 3A input ([`statistics`]).
//!
//! The AE block gives a 5x5 grid of 8-bit luma means: the 3A's luma grid. The AWB block gives
//! one white-pixel mean per channel and the number of pixels it averaged: a 1x1 colour grid.
//! The histogram is 16 luma bins. The raw blocks measure before white balance and lens
//! shading (the header does not say; this is the assumption, to be confirmed on a board).

use styx_algo::{ColourZone, LumaZone, Statistics};

use crate::regs::{AE_GRID, HIST_BIN_FRAC_BITS};

pub use lemnos_drivers_k230isp::stats::{HIST_BINS, Raw, read_raw};

/// Full scale of an AE or AWB mean (8 bits).
pub const MEAN_FULL: f64 = 255.0;

/// `snapshot` as [`Statistics`], written into `out` (its buffers reused). `pixels_per_block`
/// is the pixel count of one AE block (the window the [`crate::Params::measure`] builder
/// set): each block's mean is that many pixels at the mean's value.
pub fn statistics(snapshot: &Raw, pixels_per_block: u32, out: &mut Statistics) {
    let n = f64::from(pixels_per_block.max(1));
    let side = AE_GRID;
    out.luma = Some(styx_algo::ZoneGrid {
        width: side,
        height: side,
        zones: snapshot
            .ae
            .iter()
            .map(|&m| LumaZone {
                y: f64::from(m) / MEAN_FULL * n,
                counted: pixels_per_block,
            })
            .collect(),
    });

    let white = snapshot.awb_white;
    let w = f64::from(white);
    let [red, green, blue] = snapshot.awb_mean.map(|m| f64::from(m) / MEAN_FULL);
    out.colour.width = 1;
    out.colour.height = 1;
    out.colour.zones.clear();
    out.colour.zones.push(ColourZone {
        r: red * w,
        g: green * w,
        b: blue * w,
        counted: white,
    });

    out.histogram.set_bins(
        snapshot
            .hist
            .iter()
            .map(|&b| u64::from(b >> HIST_BIN_FRAC_BITS)),
    );
    out.focus = None;
    out.pdaf = None;
    out.before_wb = true;
    out.before_lsc = true;
}
