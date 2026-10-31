//! A statistics buffer as Styx's 3A input ([`statistics`]), from ISP21 or ISP30.

use styx_algo::{ColourZone, Statistics};

use crate::Generation;
use crate::uapi::{isp3x, isp21};

/// The colour zones' grid: the big raw AE block's 15x15 zones.
pub const AE_ZONES: u32 = 15;
/// Full scale of the AE block's red and blue zone means (10 bits, as Rockchip's 3A reads
/// them).
pub const AE_RB_FULL: f64 = 1023.0;
/// Full scale of the AE block's green zone means (12 bits).
pub const AE_G_FULL: f64 = 4095.0;

/// `buf` (the driver's statistics buffer of `generation`) as [`Statistics`], written into `out`
/// (its buffers reused): the big raw AE block's zone means as colour zones of
/// `pixels_per_zone` pixels each, the big raw histogram as the histogram. The raw blocks
/// measure before white balance and lens shading. `None` when `buf` is shorter than a
/// statistics buffer.
pub fn statistics(
    generation: Generation,
    buf: &[u8],
    pixels_per_zone: u32,
    out: &mut Statistics,
) -> Option<()> {
    // The arrays of the packed structures are copied out: they cannot be borrowed.
    match generation {
        Generation::Isp21 => {
            let stat: &isp21::rkisp_isp21_stat_buffer = buffer(buf)?;
            let (zones, bins) = (stat.params.rawae3.data, stat.params.rawhist1.hist_bin);
            fill(
                out,
                pixels_per_zone,
                zones
                    .iter()
                    .map(|z| [z.channelr_xy, z.channelg_xy, z.channelb_xy]),
                bins.iter().copied(),
            );
        }
        Generation::Isp30 => {
            let stat: &isp3x::rkisp3x_isp_stat_buffer = buffer(buf)?;
            let (zones, bins) = (stat.params.rawae3.data, stat.params.rawhist1.hist_bin);
            fill(
                out,
                pixels_per_zone,
                zones
                    .iter()
                    .map(|z| [z.channelr_xy, z.channelg_xy, z.channelb_xy]),
                bins.iter().copied(),
            );
        }
    }
    Some(())
}

/// The statistics buffer's leading `T`, if `buf` holds one.
fn buffer<T: bytemuck::Pod>(buf: &[u8]) -> Option<&T> {
    bytemuck::try_from_bytes(buf.get(..core::mem::size_of::<T>())?).ok()
}

/// `zones` (each `[r, g, b]` as the AE block reads them) and `bins` as the statistics.
fn fill(
    out: &mut Statistics,
    pixels_per_zone: u32,
    zones: impl Iterator<Item = [u16; 3]>,
    bins: impl Iterator<Item = u32>,
) {
    let n = f64::from(pixels_per_zone.max(1));
    out.colour.width = AE_ZONES;
    out.colour.height = AE_ZONES;
    out.colour.zones.clear();
    out.colour.zones.extend(zones.map(|[r, g, b]| ColourZone {
        r: f64::from(r) / AE_RB_FULL * n,
        g: f64::from(g) / AE_G_FULL * n,
        b: f64::from(b) / AE_RB_FULL * n,
        counted: pixels_per_zone,
    }));
    out.luma = None;
    out.histogram.set_bins(bins.map(u64::from));
    out.focus = None;
    out.pdaf = None;
    out.before_wb = true;
    out.before_lsc = true;
}
