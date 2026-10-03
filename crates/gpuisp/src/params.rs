//! [`IspParams`] compiled into the shaders' parameter block and tables: the same fixed-point
//! values `styx-softisp`'s integer arithmetic builds (Q12 gains, Q12 colour matrix, the
//! 4096-entry tone table), so that the GPU computes the same picture bit for bit.

use std::sync::Arc;

use styx_softisp::simd::{RowKind, ToneLut, YuvCoeffs};
use styx_softisp::{
    CfaPattern, Channel, IspError, IspParams, LensShading, RawFormat, RawPacking, StatsConfig,
    ToneCurve, YuvMatrix,
};

/// Q12 gain limit (16x), as the integer arithmetic.
const GAIN_MAX: f32 = 65535.0 / 4096.0;
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
}

impl Block {
    /// The block as bytes, for the parameter buffer.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `Block` is `repr(C)` of 4-byte integers only: no padding, any bit pattern.
        unsafe { std::slice::from_raw_parts((self as *const Self).cast::<u8>(), size_of::<Self>()) }
    }
}

/// Statistics geometry (as `styx-softisp`'s `StatsSetup`).
#[derive(Clone, Debug, PartialEq)]
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
    /// What the tables were built from: the curve, and the grid with the cell gains.
    tone_key: Option<ToneCurve>,
    lsc_key: Option<(LensShading, [u32; 4])>,
    pub stats: Option<StatsSetup>,
    /// White balance times digital gain, reported with the statistics.
    pub channel_gains: [f32; 3],
}

fn channel_index(c: Channel) -> usize {
    match c {
        Channel::Red => 0,
        Channel::Green => 1,
        Channel::Blue => 2,
    }
}

/// `round(g 4096)`, clamped to `0..=65535` (`styx-softisp`'s `q12`).
fn q12(g: f32) -> u16 {
    let y = g.clamp(0.0, GAIN_MAX) * 4096.0;
    let r = y as i32;
    (r + i32::from(y - r as f32 >= 0.5)) as u16
}

/// Sample positions of `n` output points over `nodes` grid nodes: index and Q16 weight.
fn grid_map(n: usize, nodes: usize) -> Vec<(u16, u32)> {
    (0..n)
        .map(|i| {
            if nodes < 2 || n < 2 {
                return (0, 0);
            }
            let pos = i as f64 * (nodes - 1) as f64 / (n - 1) as f64;
            let idx = (pos as usize).min(nodes - 2);
            let y = (pos - idx as f64) * 65536.0;
            let frac = y as u32 + u32::from(y - f64::from(y as u32) >= 0.5);
            (idx as u16, frac.min(65536))
        })
        .collect()
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
    /// Tables for `params`; those of `previous` are reused when what they depend on is
    /// unchanged (the 3A loop changes the gains on most frames, the curve and the lens
    /// shading less often).
    pub fn new(
        format: &RawFormat,
        params: &IspParams,
        previous: Option<&Tables>,
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
            channel_gains[channel_index(pattern.channel_at(x, y))] * full / (full - bl)
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
        let lsc_key = params.lens_shading.as_ref().map(|ls| (ls.clone(), cells));
        let (mut lsc, mut lsc_fresh) = (None, false);
        if let Some(ls) = &params.lens_shading {
            let ymap = (2 * format.width as usize * ls.height as usize).div_ceil(2) as u32;
            b.lsc_on = 1;
            b.lsc_rows = ls.height;
            b.lsc_ymap = ymap;
            lsc = match previous {
                Some(p) if p.lsc_key == lsc_key => p.lsc.clone(),
                _ => {
                    lsc_fresh = true;
                    let (words, at) = shaded_gains(ls, format, &cell_gain);
                    debug_assert_eq!(at, ymap);
                    Some(Arc::new(words))
                }
            };
        }
        b.demosaic = match params.demosaic {
            styx_softisp::Demosaic::Bilinear => 0,
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
            (Some(p), Some(_)) if p.tone_key == params.tone => (p.lut.clone(), false),
            (_, Some(curve)) => {
                let lut = ToneLut::from_curve(|x| curve.eval(x));
                (
                    Some(Arc::new(lut.full().iter().map(|&v| v as u32).collect())),
                    true,
                )
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
        let stats = params.stats.map(|c| stats_setup(c, w, h)).transpose()?;
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
            tone_key: params.tone.clone(),
            lsc_key,
            stats,
            channel_gains,
        })
    }
}

/// Q12 gains `[row parity][grid row][column]` as u16 pairs, then one word per image row
/// (grid row, Q15 weight of the next): the integer arithmetic's `GainRows::Shaded`. Returns
/// the words and where the row map starts.
fn shaded_gains(
    ls: &LensShading,
    format: &RawFormat,
    cell_gain: &dyn Fn(usize, usize) -> f32,
) -> (Vec<u32>, u32) {
    let (gw, gh) = (ls.width as usize, ls.height as usize);
    let (w, h) = (format.width as usize, format.height as usize);
    let grids = [&ls.r, &ls.g, &ls.b];
    let x_map: Vec<(usize, usize, f32)> = grid_map(w, gw)
        .into_iter()
        .map(|(i, f)| {
            let i = i as usize;
            (i.min(gw - 1), (i + 1).min(gw - 1), f as f32 / 65536.0)
        })
        .collect();
    // Per column: the weights (1 - t, t); per grid row, the two nodes gathered, then one
    // straight loop over the row (it vectorises).
    let (wa, wb): (Vec<f32>, Vec<f32>) = x_map.iter().map(|&(_, _, t)| (1.0 - t, t)).unzip();
    let (mut a, mut b) = (vec![0f32; w], vec![0f32; w]);
    let mut row = vec![0u16; w];
    let mut words: Vec<u32> = Vec::with_capacity(gh * w + h);
    for parity in 0..2 {
        let gain = [0, 1].map(|x| cell_gain(x, parity));
        let grid = [0, 1].map(|x| grids[channel_index(format.pattern.channel_at(x, parity))]);
        let g: Vec<f32> = (0..w).map(|x| gain[x & 1]).collect();
        for gy in 0..gh {
            for (x, &(i, j, _)) in x_map.iter().enumerate() {
                let node = &grid[x & 1][gy * gw..][..gw];
                a[x] = node[i];
                b[x] = node[j];
            }
            for (((((d, &a), &b), &wa), &wb), &g) in
                row.iter_mut().zip(&a).zip(&b).zip(&wa).zip(&wb).zip(&g)
            {
                *d = q12(g * (a * wa + b * wb));
            }
            words.extend(
                row.chunks_exact(2)
                    .map(|p| p[0] as u32 | (p[1] as u32) << 16),
            );
        }
    }
    let ymap = words.len() as u32;
    words.extend(
        grid_map(h, gh)
            .into_iter()
            .map(|(i, f)| i as u32 | (f >> 1) << 16),
    );
    (words, ymap)
}

fn stats_setup(c: StatsConfig, w: usize, h: usize) -> Result<StatsSetup, IspError> {
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
    let mut rows = vec![0u32; zy];
    for qy in (0..qh).step_by(step) {
        rows[qy * zy / qh] += 1;
    }
    let cols: Vec<u32> = (0..zx)
        .map(|z| ((z + 1) * qw / zx - z * qw / zx) as u32)
        .collect();
    let zone_quads = (0..zx * zy).map(|z| rows[z / zx] * cols[z % zx]).collect();
    Ok(StatsSetup {
        config: c,
        zone_quads,
        samples: rows.iter().sum::<u32>() * qw as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block's size matches the GLSL declaration (62 words).
    #[test]
    fn block_layout() {
        assert_eq!(size_of::<Block>(), 62 * 4);
    }
}
