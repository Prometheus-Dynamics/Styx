//! A statistics buffer as Styx's 3A input ([`statistics`]).

use styx_algo::{ColourZone, Statistics};

use crate::uapi::{isp2x_rawaebig_stat, rkisp3x_isp_stat_buffer};

/// The colour zones' grid: the big raw AE block's 15x15 zones.
pub const AE_ZONES: u32 = 15;
/// Full scale of the AE block's red and blue zone means (10 bits, as Rockchip's 3A reads
/// them).
pub const AE_RB_FULL: f64 = 1023.0;
/// Full scale of the AE block's green zone means (12 bits).
pub const AE_G_FULL: f64 = 4095.0;

/// `buf` (the driver's statistics buffer) as [`Statistics`], written into `out` (its buffers
/// reused): the big raw AE block's zone means as colour zones of `pixels_per_zone` pixels
/// each, the big raw histogram as the histogram. The raw blocks measure before white balance
/// and lens shading. `None` when `buf` is shorter than a statistics buffer.
pub fn statistics(buf: &[u8], pixels_per_zone: u32, out: &mut Statistics) -> Option<()> {
    let stat: &rkisp3x_isp_stat_buffer =
        bytemuck::try_from_bytes(buf.get(..core::mem::size_of::<rkisp3x_isp_stat_buffer>())?)
            .ok()?;
    let params = stat.params;
    let ae: isp2x_rawaebig_stat = params.rawae3;
    let zones = ae.data;
    let n = f64::from(pixels_per_zone.max(1));
    out.colour.width = AE_ZONES;
    out.colour.height = AE_ZONES;
    out.colour.zones.clear();
    out.colour.zones.extend(zones.iter().map(|z| ColourZone {
        r: f64::from(z.channelr_xy) / AE_RB_FULL * n,
        g: f64::from(z.channelg_xy) / AE_G_FULL * n,
        b: f64::from(z.channelb_xy) / AE_RB_FULL * n,
        counted: pixels_per_zone,
    }));
    out.luma = None;
    let hist = params.rawhist1;
    let bins = hist.hist_bin;
    out.histogram.set_bins(bins.iter().map(|&c| u64::from(c)));
    out.focus = None;
    out.pdaf = None;
    out.before_wb = true;
    out.before_lsc = true;
    Some(())
}
