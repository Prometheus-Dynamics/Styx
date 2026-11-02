//! [`IspParams`] compiled into the shaders' parameter block and tables: the same fixed-point
//! values `styx-softisp`'s integer arithmetic builds (Q12 gains, Q12 colour matrix, the
//! 4096-entry tone table), so that the GPU computes the same picture bit for bit.

use std::sync::Arc;

use styx_softisp::shared::{grid_map_into, q12};
use styx_softisp::simd::{RowKind, ToneLut, YuvCoeffs};
use styx_softisp::{
    CfaPattern, IspError, IspParams, LensShading, RawFormat, RawPacking, StatsConfig, YuvMatrix,
};

/// Q12 gain limit (16x), as the integer arithmetic.
const WORK_MAX: u16 = 4095;

/// The shaders' parameter block (`shaders/common.glsl`, `Params`): 32-bit words in the same
/// order, std430.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Block {
    pub width: u32,
    pub height: u32,
    pub in_stride: u32,
    pub in_offset: u32,
    pub packing: u32,
    pub max_value: u32,
    pub shift: u32,
    pub lsc_on: u32,
    pub lsc_rows: u32,
    pub lsc_ymap: u32,
    pub black: [u32; 4],
    pub flat_gain: [u32; 4],
    pub green_even: [u32; 2],
    pub x_is_red: [u32; 2],
    pub quad_idx: [u32; 4],
    pub demosaic: u32,
    pub ccm_on: u32,
    pub ccm: [i32; 9],
    pub tone_lut: u32,
    pub yuv_y: [u32; 3],
    pub yuv_y_offset: u32,
    pub yuv_u: [i32; 3],
    pub yuv_v: [i32; 3],
    pub out_kind: u32,
    pub out_width: u32,
    pub out_height: u32,
    pub out_offset: [u32; 3],
    pub out_stride: [u32; 3],
    pub zones_x: u32,
    pub zones_y: u32,
    pub bins: u32,
    pub saturation: u32,
    pub row_step: u32,
    /// The window's origin in the frame (0 for the whole frame).
    pub win_x: u32,
    pub win_y: u32,
}

impl Block {
    /// The block as bytes, for the parameter buffer.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `Block` is `repr(C)` of 4-byte integers only: no padding, any bit pattern.
        unsafe { std::slice::from_raw_parts((self as *const Self).cast::<u8>(), size_of::<Self>()) }
    }
}

/// Statistics geometry (as `styx-softisp`'s `StatsSetup`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StatsSetup {
    pub config: StatsConfig,
    /// Quads per zone in the sampled rows (for the zone luma means).
    pub zone_quads: Vec<u32>,
    /// Quads sampled in all.
    pub samples: u32,
}

/// Everything the shaders need for one set of parameters.
#[derive(Debug)]
pub(crate) struct Tables {
    pub block: Block,
    /// The tone table, one word per entry (`None`: plain narrowing to 8 bits).
    pub lut: Option<Arc<Vec<u32>>>,
    /// Lens shading gains as u16 pairs, then the per-row map (`None` without lens shading).
    pub lsc: Option<Arc<Vec<u32>>>,
    /// Whether `lut` and `lsc` were built anew (else they are the previous tables', already
    /// on the device).
    pub lut_fresh: bool,
    pub lsc_fresh: bool,
    /// The cell gains (bit patterns) the lens shading was built with: the grid is compared
    /// with the previous parameters' in place.
    lsc_cells: [u32; 4],
    pub stats: Option<StatsSetup>,
    /// White balance times digital gain, reported with the statistics.
    pub channel_gains: [f32; 3],
}

/// Tables of parameters gone by ([`Spare::keep`]), written in place by the next
/// [`Tables::new`], and its working buffers: parameters changing every frame (white balance,
/// lens shading, tone, as the 3A loop sets them) rebuild without allocating.
#[derive(Debug, Default)]
pub(crate) struct Spare {
    /// Two per kind: a table can come and go (a tone fit that fails) without the other one
    /// being lost.
    luts: [Option<Arc<Vec<u32>>>; 2],
    lscs: [Option<Arc<Vec<u32>>>; 2],
    stats: Option<StatsSetup>,
    tone: Option<ToneLut>,
    x_map: Vec<(usize, usize, f32)>,
    grid: Vec<(u16, u32)>,
    wa: Vec<f32>,
    wb: Vec<f32>,
    a: Vec<f32>,
    b: Vec<f32>,
    g: Vec<f32>,
    row: Vec<u16>,
}

impl Spare {
    /// Keeps `old`'s tables that nothing else holds (a table reused by the tables replacing
    /// it is not kept).
    pub fn keep(&mut self, old: Tables) {
        fn put(slots: &mut [Option<Arc<Vec<u32>>>; 2], t: Option<Arc<Vec<u32>>>) {
            if let Some(mut t) = t
                && Arc::get_mut(&mut t).is_some()
                && let Some(free) = slots.iter_mut().find(|s| s.is_none())
            {
                *free = Some(t);
            }
        }
        put(&mut self.luts, old.lut);
        put(&mut self.lscs, old.lsc);
        if old.stats.is_some() {
            self.stats = old.stats;
        }
    }
}

/// A spare table of `slots` (else a new one), emptied for writing.
fn writable(slots: &mut [Option<Arc<Vec<u32>>>; 2]) -> Arc<Vec<u32>> {
    let mut t = slots.iter_mut().find_map(Option::take).unwrap_or_default();
    Arc::get_mut(&mut t).expect("kept unshared").clear();
    t
}

fn invalid(s: impl Into<String>) -> IspError {
    IspError::InvalidParams(s.into())
}

/// The checks `styx-softisp` makes, with the same errors.
fn validate(format: &RawFormat, params: &IspParams) -> Result<(), IspError> {
    let (w, h) = (format.width as usize, format.height as usize);
    if w < 4 || h < 4 || w % 2 != 0 || h % 2 != 0 {
        return Err(IspError::Unsupported(format!(
            "{w}x{h}: Bayer frames need even sizes of at least 4x4"
        )));
    }
    let bits = format.packing.bit_depth();
    if !(8..=16).contains(&bits) {
        return Err(IspError::Unsupported(format!("{bits}-bit samples")));
    }
    let full = ((1u32 << bits) - 1) as f32;
    let black = params.black_level.map_or([0; 4], |b| b.cells());
    if black.iter().any(|&b| b as f32 >= full) {
        return Err(invalid("black level at or above full scale"));
    }
    let wb = params.white_balance.map_or([1.0; 3], |wb| wb.gains());
    if wb
        .iter()
        .chain([&params.digital_gain])
        .any(|g| !g.is_finite() || *g < 0.0)
    {
        return Err(invalid("gains must be finite and non-negative"));
    }
    if let Some(ls) = &params.lens_shading {
        let (gw, gh) = (ls.width as usize, ls.height as usize);
        if gw == 0 || gh == 0 || [&ls.r, &ls.g, &ls.b].iter().any(|g| g.len() != gw * gh) {
            return Err(invalid(format!(
                "lens shading grids must hold {gw}x{gh} gains"
            )));
        }
        if [&ls.r, &ls.g, &ls.b]
            .iter()
            .flat_map(|g| g.iter())
            .any(|g| !g.is_finite() || *g < 0.0)
        {
            return Err(invalid(
                "lens shading gains must be finite and non-negative",
            ));
        }
    }
    if let Some(c) = &params.ccm
        && c.m
            .iter()
            .flatten()
            .any(|v| !v.is_finite() || v.abs() > 4.0)
    {
        return Err(invalid("CCM coefficient outside ±4"));
    }
    Ok(())
}

impl Tables {
    /// Tables for `params`; those of `previous` (built from the parameters beside them) are
    /// reused when what they depend on is unchanged (the 3A loop changes the gains on most
    /// frames, the curve and the lens shading less often).
    pub fn new(
        format: &RawFormat,
        params: &IspParams,
        previous: Option<(&Tables, &IspParams)>,
        spare: &mut Spare,
    ) -> Result<Self, IspError> {
        validate(format, params)?;
        let (w, h) = (format.width as usize, format.height as usize);
        let bits = format.packing.bit_depth();
        let full = ((1u32 << bits) - 1) as f32;
        let pattern = format.pattern;
        let black_cells = params.black_level.map_or([0; 4], |b| b.cells());
        let wb = params.white_balance.map_or([1.0; 3], |wb| wb.gains());
        let channel_gains = wb.map(|g| g * params.digital_gain);
        // Gain of CFA cell (x & 1, y & 1), with the black level's range loss made up.
        let cell_gain = |x: usize, y: usize| {
            let bl = black_cells[pattern.cell_at(x, y)] as f32;
            channel_gains[pattern.channel_at(x, y).index()] * full / (full - bl)
        };
        let mut b = Block {
            width: w as u32,
            height: h as u32,
            packing: match format.packing {
                RawPacking::U8 => 0,
                RawPacking::U16Le { .. } => 1,
                RawPacking::Csi2Raw10 => 2,
                RawPacking::Csi2Raw12 => 3,
            },
            max_value: (1u32 << bits) - 1,
            shift: 16 - bits as u32,
            ..Block::default()
        };
        for y in 0..2 {
            for x in 0..2 {
                b.black[y * 2 + x] = black_cells[pattern.cell_at(x, y)] as u32;
                b.flat_gain[y * 2 + x] = q12(cell_gain(x, y)) as u32;
            }
            let kind = RowKind::of(pattern, y);
            b.green_even[y] = kind.green_even as u32;
            b.x_is_red[y] = kind.x_is_red as u32;
        }
        b.quad_idx = match pattern {
            CfaPattern::Rggb => [0, 1, 2, 3],
            CfaPattern::Bggr => [3, 1, 2, 0],
            CfaPattern::Grbg => [1, 0, 3, 2],
            CfaPattern::Gbrg => [2, 0, 3, 1],
        };
        let cells = [0, 1, 2, 3].map(|i| cell_gain(i & 1, i >> 1).to_bits());
        let (mut lsc, mut lsc_fresh) = (None, false);
        if let Some(ls) = &params.lens_shading {
            let ymap = (2 * format.width as usize * ls.height as usize).div_ceil(2) as u32;
            b.lsc_on = 1;
            b.lsc_rows = ls.height;
            b.lsc_ymap = ymap;
            lsc = match previous {
                Some((p, prev))
                    if prev.lens_shading == params.lens_shading && p.lsc_cells == cells =>
                {
                    p.lsc.clone()
                }
                _ => {
                    lsc_fresh = true;
                    let mut words = writable(&mut spare.lscs);
                    let words_mut = Arc::get_mut(&mut words).expect("unshared");
                    let at = shaded_gains(ls, format, &cell_gain, spare, words_mut);
                    debug_assert_eq!(at, ymap);
                    Some(words)
                }
            };
        }
        // The GPU has no edge-directed demosaic (nor the output stages: `IspParams::denoise` and
        // `sharpen` are not applied here): it runs the bilinear one for it, as the CPU's default.
        b.demosaic = match params.demosaic {
            styx_softisp::Demosaic::Bilinear | styx_softisp::Demosaic::EdgeDirected => 0,
            styx_softisp::Demosaic::Mhc => 1,
        };
        if let Some(c) = &params.ccm {
            b.ccm_on = 1;
            for (k, v) in c.m.iter().flatten().enumerate() {
                b.ccm[k] = (v * 4096.0).round() as i16 as i32;
            }
        }
        b.tone_lut = u32::from(params.tone.is_some());
        let (lut, lut_fresh) = match (previous, &params.tone) {
            (Some((p, prev)), Some(_)) if prev.tone == params.tone => (p.lut.clone(), false),
            (_, Some(curve)) => {
                let tone = spare
                    .tone
                    .get_or_insert_with(|| ToneLut::from_nodes([0; 257]));
                tone.set_curve(|x| curve.eval(x));
                let mut lut = writable(&mut spare.luts);
                Arc::get_mut(&mut lut)
                    .expect("unshared")
                    .extend(tone.full().iter().map(|&v| v as u32));
                (Some(lut), true)
            }
            (_, None) => (None, false),
        };
        let yuv = match params.yuv {
            YuvMatrix::Bt601Full => YuvCoeffs::BT601_FULL,
            YuvMatrix::Bt709Limited => YuvCoeffs::BT709_LIMITED,
        };
        b.yuv_y = yuv.y.map(u32::from);
        b.yuv_y_offset = yuv.y_offset as u32;
        b.yuv_u = yuv.u.map(i32::from);
        b.yuv_v = yuv.v.map(i32::from);
        let stats = match params.stats {
            Some(c) => {
                let mut s = spare.stats.take().unwrap_or_default();
                stats_setup_into(c, w, h, &mut s)?;
                Some(s)
            }
            None => None,
        };
        if let Some(s) = &stats {
            b.zones_x = s.config.zones_x;
            b.zones_y = s.config.zones_y;
            b.bins = s.config.histogram_bins;
            b.saturation = (s.config.saturation.clamp(0.0, 1.0) * WORK_MAX as f32).round() as u32;
            b.row_step = s.config.row_step;
        }
        Ok(Self {
            block: b,
            lut,
            lsc,
            lut_fresh,
            lsc_fresh,
            lsc_cells: cells,
            stats,
            channel_gains,
        })
    }
}

/// Q12 gains `[row parity][grid row][column]` as u16 pairs, then one word per image row
/// (grid row, Q15 weight of the next): the integer arithmetic's `GainRows::Shaded`, written
/// into `words` (with `spare`'s working buffers). Returns where the row map starts.
fn shaded_gains(
    ls: &LensShading,
    format: &RawFormat,
    cell_gain: &dyn Fn(usize, usize) -> f32,
    spare: &mut Spare,
    words: &mut Vec<u32>,
) -> u32 {
    let (gw, gh) = (ls.width as usize, ls.height as usize);
    let (w, h) = (format.width as usize, format.height as usize);
    let grids = [&ls.r, &ls.g, &ls.b];
    let Spare {
        x_map,
        grid: nodes,
        wa,
        wb,
        a,
        b,
        g,
        row,
        ..
    } = spare;
    grid_map_into(nodes, w, gw);
    x_map.clear();
    x_map.extend(nodes.iter().map(|&(i, f)| {
        let i = i as usize;
        (i.min(gw - 1), (i + 1).min(gw - 1), f as f32 / 65536.0)
    }));
    // Per column: the weights (1 - t, t); per grid row, the two nodes gathered, then one
    // straight loop over the row (it vectorises).
    wa.clear();
    wa.extend(x_map.iter().map(|&(_, _, t)| 1.0 - t));
    wb.clear();
    wb.extend(x_map.iter().map(|&(_, _, t)| t));
    for v in [&mut *a, &mut *b, &mut *g] {
        v.clear();
        v.resize(w, 0.0);
    }
    row.clear();
    row.resize(w, 0);
    words.reserve(gh * w + h);
    for parity in 0..2 {
        let gain = [0, 1].map(|x| cell_gain(x, parity));
        let grid = [0, 1].map(|x| grids[format.pattern.channel_at(x, parity).index()]);
        for (x, v) in g.iter_mut().enumerate() {
            *v = gain[x & 1];
        }
        for gy in 0..gh {
            for (x, &(i, j, _)) in x_map.iter().enumerate() {
                let node = &grid[x & 1][gy * gw..][..gw];
                a[x] = node[i];
                b[x] = node[j];
            }
            for (((((d, &a), &b), &wa), &wb), &g) in row
                .iter_mut()
                .zip(a.iter())
                .zip(b.iter())
                .zip(wa.iter())
                .zip(wb.iter())
                .zip(g.iter())
            {
                *d = q12(g * (a * wa + b * wb));
            }
            words.extend(
                row.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| p[0] as u32 | (p[1] as u32) << 16),
            );
        }
    }
    let ymap = words.len() as u32;
    grid_map_into(nodes, h, gh);
    words.extend(nodes.iter().map(|&(i, f)| i as u32 | (f >> 1) << 16));
    ymap
}

/// The statistics setup for `c` on a `w` x `h` frame, written into `out` (its buffer reused).
fn stats_setup_into(
    c: StatsConfig,
    w: usize,
    h: usize,
    out: &mut StatsSetup,
) -> Result<(), IspError> {
    let (qw, qh) = (w / 2, h / 2);
    if c.zones_x == 0 || c.zones_y == 0 || c.zones_x as usize > qw || c.zones_y as usize > qh {
        return Err(invalid(format!(
            "{}x{} statistics zones for {qw}x{qh} quads",
            c.zones_x, c.zones_y
        )));
    }
    if !(1..=4096).contains(&c.histogram_bins) || c.row_step == 0 {
        return Err(invalid(
            "histogram bins must be 1 to 4096 and the row step at least 1",
        ));
    }
    let (zx, zy) = (c.zones_x as usize, c.zones_y as usize);
    let step = c.row_step as usize;
    // Sampled quad rows of zone row `r`.
    let rows = |r: usize| {
        (0..qh)
            .step_by(step)
            .filter(|&qy| qy * zy / qh == r)
            .count() as u32
    };
    let cols = |z: usize| ((z + 1) * qw / zx - z * qw / zx) as u32;
    out.zone_quads.clear();
    for r in 0..zy {
        let n = rows(r);
        out.zone_quads.extend((0..zx).map(|z| n * cols(z)));
    }
    out.config = c;
    out.samples = (0..qh).step_by(step).count() as u32 * qw as u32;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block's size matches the GLSL declaration (64 words).
    #[test]
    fn block_layout() {
        assert_eq!(size_of::<Block>(), 64 * 4);
    }
}
