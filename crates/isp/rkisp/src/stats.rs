//! A statistics buffer as Styx's 3A input ([`statistics`]): the big raw AE block's zone means as
//! colour zones and the big raw histogram, decoded from the library's ISP21 or ISP30 layout.

use core::mem::{offset_of, size_of};

use styx_aaa::{ColourZone, Statistics};
use styx_lemnos::rkisp::Generation;
use styx_lemnos::rkisp::uapi::{self, isp3x, isp21};

use crate::bytes::{get_u16, get_u32};

/// The colour zones' grid: the big raw AE block's 15x15 zones.
pub const AE_ZONES: u32 = 15;
/// Zones the AE block reports (`AE_ZONES` squared).
const ZONES: usize = (AE_ZONES * AE_ZONES) as usize;
/// Bins the big raw histogram reports.
const BINS: usize = 256;
/// Full scale of the AE block's red and blue zone means (10 bits, as the 3A reads them).
pub const AE_RB_FULL: f64 = 1023.0;
/// Full scale of the AE block's green zone means (12 bits).
pub const AE_G_FULL: f64 = 4095.0;

/// Where one generation's statistics are in its buffer: the buffer's size, the AE zones (their
/// count is [`ZONES`]; each is `zone_bytes` long) with the offsets of the channel means inside a
/// zone, and the histogram's bins.
struct Where {
    len: usize,
    zones: usize,
    zone_bytes: usize,
    r: usize,
    g: usize,
    b: usize,
    hist: usize,
}

/// The layout of `generation`'s statistics buffer; `None` for ISP35 (not in the library's module
/// set this crate maps; see the crate documentation).
fn layout(generation: Generation) -> Option<Where> {
    Some(match generation {
        Generation::Isp21 => {
            type Buf = isp21::rkisp_isp21_stat_buffer;
            type Zone = isp21::isp2x_rawae_meas_data;
            Where {
                len: uapi::ISP21_STATS_BYTES,
                zones: offset_of!(Buf, params.rawae3.data),
                zone_bytes: size_of::<Zone>(),
                r: offset_of!(Zone, channelr_xy),
                g: offset_of!(Zone, channelg_xy),
                b: offset_of!(Zone, channelb_xy),
                hist: offset_of!(Buf, params.rawhist1.hist_bin),
            }
        }
        Generation::Isp30 => {
            type Buf = isp3x::rkisp3x_isp_stat_buffer;
            type Zone = isp3x::isp2x_rawae_meas_data;
            Where {
                len: uapi::ISP30_STATS_BYTES,
                zones: offset_of!(Buf, params.rawae3.data),
                zone_bytes: size_of::<Zone>(),
                r: offset_of!(Zone, channelr_xy),
                g: offset_of!(Zone, channelg_xy),
                b: offset_of!(Zone, channelb_xy),
                hist: offset_of!(Buf, params.rawhist1.hist_bin),
            }
        }
        Generation::Isp35 => return None,
    })
}

/// `buf` (the ISP's statistics buffer of `generation`, from its statistics stream) as
/// [`Statistics`], written into `out` (its buffers reused): the AE block's zone means as colour
/// zones of `pixels_per_zone` pixels each, the histogram as the histogram. The raw blocks measure
/// before white balance and lens shading. `None` when `buf` is shorter than a statistics buffer,
/// or for ISP35.
pub fn statistics(
    generation: Generation,
    buf: &[u8],
    pixels_per_zone: u32,
    out: &mut Statistics,
) -> Option<()> {
    let w = layout(generation)?;
    let buf = buf.get(..w.len)?;
    let n = f64::from(pixels_per_zone.max(1));
    out.colour.width = AE_ZONES;
    out.colour.height = AE_ZONES;
    out.colour.zones.clear();
    out.colour.zones.extend((0..ZONES).map(|i| {
        let at = w.zones + i * w.zone_bytes;
        ColourZone {
            r: f64::from(get_u16(buf, at + w.r)) / AE_RB_FULL * n,
            g: f64::from(get_u16(buf, at + w.g)) / AE_G_FULL * n,
            b: f64::from(get_u16(buf, at + w.b)) / AE_RB_FULL * n,
            counted: pixels_per_zone,
        }
    }));
    out.luma = None;
    out.histogram
        .set_bins((0..BINS).map(|i| u64::from(get_u32(buf, w.hist + 4 * i))));
    out.focus = None;
    out.pdaf = None;
    out.before_wb = true;
    out.before_lsc = true;
    Some(())
}
