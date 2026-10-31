//! The statistics the ISP measured, read from its registers ([`read_raw`]) and turned into
//! Styx's 3A input ([`statistics`]).
//!
//! The AE block gives a 5x5 grid of 8-bit luma means: the 3A's luma grid. The AWB block gives
//! one white-pixel mean per channel and the number of pixels it averaged: a 1x1 colour grid.
//! The histogram is 16 luma bins. The raw blocks measure before white balance and lens
//! shading (the header does not say; this is the assumption, to be confirmed on a board).

use styx_algo::{ColourZone, LumaZone, Statistics};

use crate::params::RegisterAccess;
use crate::regs::{
    AE_GRID, AE_MEAN_0, AE_MEAN_MASK, AWB_WHITE_CNT_MASK, HIST_BIN_0, HIST_BIN_FRAC_BITS,
    HIST_BIN_MASK, ISP_AWB_MEAN, ISP_AWB_WHITE_CNT,
};

/// Bins of the luma histogram.
pub const HIST_BINS: usize = 16;
/// Full scale of an AE or AWB mean (8 bits).
pub const MEAN_FULL: f64 = 255.0;

/// A statistics snapshot as the registers hold it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Raw {
    /// The AE block means, row-major (`y * 5 + x`), 8 bits each.
    pub ae: [u8; 25],
    /// The histogram bins as the registers hold them: 16 integer and 4 fractional bits.
    pub hist: [u32; HIST_BINS],
    /// The AWB means of red, green and blue (RGB measurement mode), 8 bits each.
    pub awb_mean: [u8; 3],
    /// Pixels the AWB block averaged.
    pub awb_white: u32,
}

/// Reads a statistics snapshot: the 25 AE means, the 16 histogram bins and the AWB means and
/// count.
pub fn read_raw<R: RegisterAccess>(regs: &mut R) -> Result<Raw, R::Error> {
    let mut raw = Raw::default();
    for (i, m) in raw.ae.iter_mut().enumerate() {
        *m = (regs.read(AE_MEAN_0 + 4 * i as u32)? & AE_MEAN_MASK) as u8;
    }
    for (i, b) in raw.hist.iter_mut().enumerate() {
        *b = regs.read(HIST_BIN_0 + 4 * i as u32)? & HIST_BIN_MASK;
    }
    let mean = regs.read(ISP_AWB_MEAN)?;
    // RGB measurement mode: red in bits 7..0, blue in 15..8, green in 23..16.
    raw.awb_mean = [mean as u8, (mean >> 16) as u8, (mean >> 8) as u8];
    raw.awb_white = regs.read(ISP_AWB_WHITE_CNT)? & AWB_WHITE_CNT_MASK;
    Ok(raw)
}

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
