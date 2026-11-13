//! The front end's statistics, as Styx's 3A statistics: the library's layouts parsed, then the
//! colour zones, the luma histogram and the focus figures the controller takes.

use styx_aaa::{ColourZone, Statistics, ZoneGrid};
use styx_lemnos::pisp::stats::Statistics as RawFrame;
use styx_lemnos::pisp::uapi::{AWB_STATS_SIZE, CDAF_STATS_SIZE, RawStatistics};

use crate::PispError;

/// The front end's sums are in 1/65536 units.
const PISP_SCALE: f64 = 65536.0;

/// Parses one statistics buffer (the library's layout) into `out`, reusing its buffers. A
/// buffer shorter than the layout is an error, and `out` is left as it was.
pub fn parse_into(raw: &mut RawFrame, bytes: &[u8], out: &mut Statistics) -> Result<(), PispError> {
    raw.parse_into(bytes)
        .map_err(|e| PispError::Stats(format!("{e}")))?;
    from_raw(&raw.raw, out);
    Ok(())
}

/// The front end's statistics written into `out`: the colour zones (before white balance and
/// lens shading), the histogram and the focus grid.
pub fn from_raw(raw: &RawStatistics, out: &mut Statistics) {
    let side = AWB_STATS_SIZE as u32;
    out.colour.width = side;
    out.colour.height = side;
    out.colour.zones.clear();
    out.colour
        .zones
        .extend(raw.awb.zones.iter().map(|z| ColourZone {
            r: f64::from(z.r_sum) / PISP_SCALE,
            g: f64::from(z.g_sum) / PISP_SCALE,
            b: f64::from(z.b_sum) / PISP_SCALE,
            counted: z.counted,
        }));
    out.luma = None;
    out.histogram
        .set_bins(raw.agc.histogram.iter().map(|&c| u64::from(c)));
    let focus_side = CDAF_STATS_SIZE as u32;
    let grid = out.focus.get_or_insert_with(|| ZoneGrid {
        width: 0,
        height: 0,
        zones: Vec::new(),
    });
    grid.width = focus_side;
    grid.height = focus_side;
    grid.zones.clear();
    grid.zones.extend(raw.cdaf.foms.iter().map(|&f| f as f64));
    out.before_wb = true;
    out.before_lsc = true;
}
