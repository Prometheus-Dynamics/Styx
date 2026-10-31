//! A statistics buffer of an ISP32 or ISP32-lite as Styx's 3A input ([`statistics`]).

use styx_algo::{ColourZone, Statistics};

use super::Variant;
use crate::uapi::isp32::{rkisp32_isp_stat_buffer, rkisp32_lite_stat_buffer};
use crate::{AE_G_FULL, AE_RB_FULL, AE_ZONES};

/// `buf` (the driver's statistics buffer for `variant`) as [`Statistics`], written into `out`
/// (its buffers reused): the big raw AE block's 15x15 zone means as colour zones of
/// `pixels_per_zone` pixels each, and the big raw histogram as the histogram (`rawhist1` on
/// ISP32, `rawhist3` on ISP32-lite). The raw blocks measure before white balance and lens
/// shading. `None` when `buf` is shorter than the variant's statistics buffer.
///
/// The zone scales are ISP30's ([`AE_RB_FULL`], [`AE_G_FULL`]). The ISP32 zones are 10-bit red
/// and blue and 12-bit green, packed in 4 bytes. The ISP32-lite zones are the same measurements
/// in 16-bit fields (`isp2x_rawae_meas_data`), read with the same scales: the header does not
/// say the lite's full scale, so this is to confirm on the device.
pub fn statistics(
    variant: Variant,
    buf: &[u8],
    pixels_per_zone: u32,
    out: &mut Statistics,
) -> Option<()> {
    let n = f64::from(pixels_per_zone.max(1));
    let (zones, bins): ([(f64, f64, f64); 225], [u32; 256]) = match variant {
        Variant::Isp32 => {
            let stat: &rkisp32_isp_stat_buffer = bytemuck::try_from_bytes(
                buf.get(..core::mem::size_of::<rkisp32_isp_stat_buffer>())?,
            )
            .ok()?;
            let ae = stat.params.rawae3_0;
            let hist = stat.params.rawhist1;
            let zones: [(f64, f64, f64); 225] = core::array::from_fn(|i| {
                let z = ae.data[i];
                (
                    f64::from(z.channelr_xy()) / AE_RB_FULL,
                    f64::from(z.channelg_xy()) / AE_G_FULL,
                    f64::from(z.channelb_xy()) / AE_RB_FULL,
                )
            });
            (zones, hist.hist_bin)
        }
        Variant::Isp32Lite => {
            let stat: &rkisp32_lite_stat_buffer = bytemuck::try_from_bytes(
                buf.get(..core::mem::size_of::<rkisp32_lite_stat_buffer>())?,
            )
            .ok()?;
            let ae = stat.params.rawae3;
            let hist = stat.params.rawhist3;
            let zones: [(f64, f64, f64); 225] = core::array::from_fn(|i| {
                let z = ae.data[i];
                (
                    f64::from(z.channelr_xy) / AE_RB_FULL,
                    f64::from(z.channelg_xy) / AE_G_FULL,
                    f64::from(z.channelb_xy) / AE_RB_FULL,
                )
            });
            (zones, hist.hist_bin)
        }
    };
    out.colour.width = AE_ZONES;
    out.colour.height = AE_ZONES;
    out.colour.zones.clear();
    out.colour
        .zones
        .extend(zones.iter().map(|&(r, g, b)| ColourZone {
            r: r * n,
            g: g * n,
            b: b * n,
            counted: pixels_per_zone,
        }));
    out.luma = None;
    out.histogram.set_bins(bins.iter().map(|&c| u64::from(c)));
    out.focus = None;
    out.pdaf = None;
    out.before_wb = true;
    out.before_lsc = true;
    Some(())
}
