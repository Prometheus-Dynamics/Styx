//! Parameters compiled into the fixed-point tables the row loop uses.

use alloc::{boxed::Box, format, vec, vec::Vec};

use crate::format::{CfaPattern, Channel, RawFormat};
use crate::params::ToneCurve;
use crate::params::{Arithmetic, Demosaic, IspParams, LensShading, YuvMatrix};
use crate::prepare_half::HalfPrep;
use crate::simd::poly::PolyTone;
use crate::simd::scalar::WORK_MAX;
use crate::simd::{ToneLut, YuvCoeffs};
use crate::{IspError, StatsConfig};
#[cfg(not(feature = "std"))]
use styx_core::math::Float as _;

/// Q12 gain limit (16x).
const GAIN_MAX: f32 = 65535.0 / 4096.0;

/// Per-row gains: one row per row parity, or, with lens shading, one per parity and grid row
/// to interpolate between.
#[derive(Debug)]
pub(crate) enum GainRows {
    Flat([Vec<u16>; 2]),
    Shaded {
        /// `rows[parity][grid row]`, each `width` Q12 gains.
        rows: [Vec<Vec<u16>>; 2],
        /// Per image row: lower grid row and Q16 weight of the next.
        y_map: Vec<(u16, u32)>,
    },
}

#[derive(Debug)]
pub(crate) struct Prepared {
    pub width: usize,
    pub height: usize,
    pub pattern: CfaPattern,
    pub demosaic: Demosaic,
    /// The per-pixel stages' tables.
    pub arith: Arith,
    pub yuv: YuvCoeffs,
    pub stats: Option<StatsSetup>,
    /// White balance times digital gain, reported with the statistics.
    pub channel_gains: [f32; 3],
}

/// Tables of the arithmetic in use.
#[derive(Debug)]
pub(crate) enum Arith {
    Int(IntPrep),
    Half(HalfPrep),
}

/// [`Arithmetic::Int`]'s tables.
#[derive(Debug)]
pub(crate) struct IntPrep {
    /// Left shift putting input samples' top bit at bit 15.
    pub shift: u32,
    /// `black[row parity][column parity]`.
    pub black: [[u16; 2]; 2],
    pub gains: GainRows,
    /// Q12 colour matrix.
    pub ccm: Option<[i16; 9]>,
    pub lut: Option<ToneLut>,
    /// The table as quadratics ([`Arithmetic::IntPolyTone`]), when they fit it.
    pub poly: Option<Box<PolyTone>>,
    /// The curve `lut` came from, and whether quadratics were wanted for it.
    curve: (Option<ToneCurve>, bool),
}

/// Statistics geometry.
#[derive(Clone, Debug)]
pub(crate) struct StatsSetup {
    pub config: StatsConfig,
    /// Zone column boundaries in quads (`zones_x + 1` entries).
    pub col_edges: Vec<usize>,
    pub quad_rows: usize,
    pub saturation: u16,
}

pub(crate) fn channel_index(c: Channel) -> usize {
    match c {
        Channel::Red => 0,
        Channel::Green => 1,
        Channel::Blue => 2,
    }
}

/// `round(g 4096)` (halves away from zero), clamped to `0..=65535`. Truncation plus a
/// comparison: the same result as `f32::round`, which x86 without SSE4.1 calls `roundf` for
/// (a lens shading table rebuild took 0.27 instead of 0.03 ms on Zen 3).
#[inline]
fn q12(g: f32) -> u16 {
    let y = g.clamp(0.0, GAIN_MAX) * 4096.0;
    // Exact for 0 <= y < 2^24: `y - trunc(y)` is representable.
    let r = y as i32;
    (r + i32::from(y - r as f32 >= 0.5)) as u16
}

/// Sample positions of `n` output points spread over `nodes` grid nodes: index and Q16 weight.
pub(crate) fn grid_map(n: usize, nodes: usize) -> Vec<(u16, u32)> {
    (0..n)
        .map(|i| {
            if nodes < 2 || n < 2 {
                return (0, 0);
            }
            // `floor` and `round` (halves away from zero) of non-negative values by
            // truncation: x86 without SSE4.1 calls libm for them.
            let pos = i as f64 * (nodes - 1) as f64 / (n - 1) as f64;
            let idx = (pos as usize).min(nodes - 2);
            let y = (pos - idx as f64) * 65536.0;
            let frac = y as u32 + u32::from(y - f64::from(y as u32) >= 0.5);
            (idx as u16, frac.min(65536))
        })
        .collect()
}

impl Prepared {
    /// Tables for `params`; lens shading tables of `previous` are kept when its grid differs
    /// from the new one by at most `lsc_tolerance` (relative) in every node.
    pub fn new(
        format: &RawFormat,
        params: &IspParams,
        previous: Option<&Prepared>,
        lsc_tolerance: f32,
    ) -> Result<Self, IspError> {
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
        let black_cells = params.black_level.map_or([0; 4], |b| b.cells());
        if black_cells.iter().any(|&b| b as f32 >= full) {
            return Err(IspError::InvalidParams(
                "black level at or above full scale".into(),
            ));
        }
        let wb = params.white_balance.map_or([1.0; 3], |wb| wb.gains());
        let digital = params.digital_gain;
        if wb
            .iter()
            .chain([&digital])
            .any(|g| !g.is_finite() || *g < 0.0)
        {
            return Err(IspError::InvalidParams(
                "gains must be finite and non-negative".into(),
            ));
        }
        let channel_gains = wb.map(|g| g * digital);
        let pattern = format.pattern;
        if let Some(ls) = &params.lens_shading {
            check_lens_shading(ls)?;
        }
        if let Some(c) = &params.ccm
            && c.m
                .iter()
                .flatten()
                .any(|v| !v.is_finite() || v.abs() > 4.0)
        {
            return Err(IspError::InvalidParams("CCM coefficient outside ±4".into()));
        }
        let previous_half = previous.and_then(|p| match &p.arith {
            Arith::Half(h) => Some(h),
            Arith::Int(_) => None,
        });
        let arith = match HalfPrep::eligible(format, params, previous_half) {
            Some(tone) => Arith::Half(HalfPrep::new(
                format,
                params,
                tone,
                channel_gains,
                previous_half,
                lsc_tolerance,
            )),
            None => {
                let previous = previous.and_then(|p| match &p.arith {
                    Arith::Int(i) => Some(i),
                    Arith::Half(_) => None,
                });
                Arith::Int(IntPrep::new(format, params, channel_gains, previous)?)
            }
        };
        let yuv = match params.yuv {
            YuvMatrix::Bt601Full => YuvCoeffs::BT601_FULL,
            YuvMatrix::Bt709Limited => YuvCoeffs::BT709_LIMITED,
        };
        let stats = params.stats.map(|c| stats_setup(c, w, h)).transpose()?;
        Ok(Self {
            width: w,
            height: h,
            pattern,
            demosaic: params.demosaic,
            arith,
            yuv,
            stats,
            channel_gains,
        })
    }

    /// The arithmetic in use.
    pub fn arithmetic(&self) -> Arithmetic {
        match &self.arith {
            Arith::Int(i) if i.poly.is_some() => Arithmetic::IntPolyTone,
            Arith::Int(_) => Arithmetic::Int,
            Arith::Half(_) => Arithmetic::Half,
        }
    }
}

impl IntPrep {
    fn new(
        format: &RawFormat,
        params: &IspParams,
        channel_gains: [f32; 3],
        previous: Option<&IntPrep>,
    ) -> Result<Self, IspError> {
        let w = format.width as usize;
        let bits = format.packing.bit_depth();
        let full = ((1u32 << bits) - 1) as f32;
        let black_cells = params.black_level.map_or([0; 4], |b| b.cells());
        let pattern = format.pattern;
        // Gain of CFA cell (x & 1, y & 1), with the black level's range loss made up.
        let cell_gain = |x: usize, y: usize| {
            let bl = black_cells[pattern.cell_at(x, y)] as f32;
            channel_gains[channel_index(pattern.channel_at(x, y))] * full / (full - bl)
        };
        let black = [0, 1].map(|y| [0, 1].map(|x| black_cells[pattern.cell_at(x, y)]));
        let gains = match &params.lens_shading {
            None => GainRows::Flat([0, 1].map(|y| (0..w).map(|x| q12(cell_gain(x, y))).collect())),
            Some(ls) => shaded_gains(ls, format, &cell_gain)?,
        };
        let ccm = params.ccm.map(|c| {
            let mut m = [0i16; 9];
            for (k, v) in c.m.iter().flatten().enumerate() {
                m[k] = (v * 4096.0).round() as i16;
            }
            m
        });
        // Never without feature `poly-tone`.
        let poly_wanted = cfg!(feature = "poly-tone")
            && match params.arithmetic {
                Arithmetic::IntPolyTone => true,
                Arithmetic::Auto => crate::simd::poly_preferred(),
                Arithmetic::Int | Arithmetic::Half => false,
            };
        // The same curve keeps its table and quadratics (fitting costs tens of microseconds).
        let (lut, poly) = match previous {
            Some(p) if p.curve.0 == params.tone && p.curve.1 == poly_wanted => {
                (p.lut.clone(), p.poly.clone())
            }
            _ => {
                let lut = params
                    .tone
                    .as_ref()
                    .map(|curve| ToneLut::from_curve(|x| curve.eval(x)));
                let poly = lut
                    .as_ref()
                    .filter(|_| poly_wanted)
                    .and_then(|l| PolyTone::fit(l.full()))
                    .map(Box::new);
                (lut, poly)
            }
        };
        Ok(Self {
            shift: 16 - bits as u32,
            black,
            gains,
            ccm,
            lut,
            poly,
            curve: (params.tone.clone(), poly_wanted),
        })
    }
}

/// Lens shading grids must be complete and hold finite, non-negative gains.
fn check_lens_shading(ls: &LensShading) -> Result<(), IspError> {
    let (gw, gh) = (ls.width as usize, ls.height as usize);
    let nodes = gw * gh;
    if gw == 0 || gh == 0 || [&ls.r, &ls.g, &ls.b].iter().any(|g| g.len() != nodes) {
        return Err(IspError::InvalidParams(format!(
            "lens shading grids must hold {gw}x{gh} gains"
        )));
    }
    if [&ls.r, &ls.g, &ls.b]
        .iter()
        .flat_map(|g| g.iter())
        .any(|g| !g.is_finite() || *g < 0.0)
    {
        return Err(IspError::InvalidParams(
            "lens shading gains must be finite and non-negative".into(),
        ));
    }
    Ok(())
}

fn shaded_gains(
    ls: &LensShading,
    format: &RawFormat,
    cell_gain: &dyn Fn(usize, usize) -> f32,
) -> Result<GainRows, IspError> {
    let (gw, gh) = (ls.width as usize, ls.height as usize);
    let (w, h) = (format.width as usize, format.height as usize);
    let grids = [&ls.r, &ls.g, &ls.b];
    let x_map = grid_map(w, gw);
    // Columns of each parity: their weights of the second grid node, and runs of them between
    // the same two grid nodes.
    let lanes = [0, 1].map(|p| {
        let cols: Vec<(usize, usize, f32)> = x_map
            .iter()
            .skip(p)
            .step_by(2)
            .map(|&(i, f)| {
                let i = i as usize;
                (i.min(gw - 1), (i + 1).min(gw - 1), f as f32 / 65536.0)
            })
            .collect();
        let mut runs: Vec<(usize, usize, usize, usize)> = Vec::new();
        for (k, &(i, j, _)) in cols.iter().enumerate() {
            match runs.last_mut() {
                Some(r) if (r.2, r.3) == (i, j) => r.1 = k + 1,
                _ => runs.push((k, k + 1, i, j)),
            }
        }
        (cols.iter().map(|c| c.2).collect::<Vec<f32>>(), runs)
    });
    // Each column's two nodes, filled run by run, so that the arithmetic is one vector loop
    // over the row (0.21 instead of 0.31 ms per rebuild on the Cortex-A76 with the rounding
    // below; 0.25 vectorised per run).
    let (mut a, mut b) = (vec![0.0f32; w.div_ceil(2)], vec![0.0f32; w.div_ceil(2)]);
    let rows = [0, 1].map(|parity| {
        // Everything but the shading depends on the column's parity only.
        let gain = [0, 1].map(|x| cell_gain(x, parity));
        let grid = [0, 1].map(|x| grids[channel_index(format.pattern.channel_at(x, parity))]);
        let mut half = [vec![0u16; w.div_ceil(2)], vec![0u16; w / 2]];
        (0..gh)
            .map(|gy| {
                for (p, out) in half.iter_mut().enumerate() {
                    let node = &grid[p][gy * gw..][..gw];
                    let (ts, runs) = &lanes[p];
                    for &(k0, k1, i, j) in runs {
                        a[k0..k1].fill(node[i]);
                        b[k0..k1].fill(node[j]);
                    }
                    q12_row(out, &a, &b, ts, gain[p]);
                }
                let mut row = vec![0u16; w];
                for ((pair, &even), &odd) in row
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(&half[0])
                    .zip(&half[1])
                {
                    pair[0] = even;
                    pair[1] = odd;
                }
                if w % 2 == 1 {
                    row[w - 1] = half[0][w / 2];
                }
                row
            })
            .collect()
    });
    Ok(GainRows::Shaded {
        rows,
        y_map: grid_map(h, gh),
    })
}

/// `out[k] = q12(g (a[k] (1 - t[k]) + b[k] t[k]))`.
fn q12_row(out: &mut [u16], a: &[f32], b: &[f32], t: &[f32], g: f32) {
    for (((d, &a), &b), &t) in out.iter_mut().zip(a).zip(b).zip(t) {
        *d = q12(g * (a * (1.0 - t) + b * t));
    }
}

fn stats_setup(c: StatsConfig, w: usize, h: usize) -> Result<StatsSetup, IspError> {
    let (qw, qh) = (w / 2, h / 2);
    if c.zones_x == 0 || c.zones_y == 0 || c.zones_x as usize > qw || c.zones_y as usize > qh {
        return Err(IspError::InvalidParams(format!(
            "{}x{} statistics zones for {qw}x{qh} quads",
            c.zones_x, c.zones_y
        )));
    }
    if !(1..=4096).contains(&c.histogram_bins) || c.row_step == 0 {
        return Err(IspError::InvalidParams(
            "histogram bins must be 1 to 4096 and the row step at least 1".into(),
        ));
    }
    let zx = c.zones_x as usize;
    Ok(StatsSetup {
        config: c,
        col_edges: (0..=zx).map(|z| z * qw / zx).collect(),
        quad_rows: qh,
        saturation: (c.saturation.clamp(0.0, 1.0) * WORK_MAX as f32).round() as u16,
    })
}

impl GainRows {
    /// The gains of image row `y`, columns `x0 .. x0 + n` (`scratch` receives interpolated
    /// lens shading rows).
    pub fn row<'a>(
        &'a self,
        y: usize,
        x0: usize,
        n: usize,
        scratch: &'a mut Vec<u16>,
    ) -> &'a [u16] {
        match self {
            Self::Flat(rows) => &rows[y & 1][x0..x0 + n],
            Self::Shaded { rows, y_map } => {
                let rows = &rows[y & 1];
                let (i, f) = y_map[y];
                let a = &rows[i as usize][x0..x0 + n];
                let b = &rows[(i as usize + 1).min(rows.len() - 1)][x0..x0 + n];
                scratch.resize(a.len(), 0);
                // Q15 so that the product fits 32 bits.
                let f = (f >> 1) as i32;
                for ((d, &a), &b) in scratch.iter_mut().zip(a).zip(b) {
                    *d = (a as i32 + (((b as i32 - a as i32) * f) >> 15)) as u16;
                }
                scratch
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `q12` and `grid_map` round by truncation: the same as `round` (and `floor`) for every
    /// value they meet, halves included.
    #[test]
    fn rounding_by_truncation_matches_round() {
        let old = |g: f32| (g.clamp(0.0, GAIN_MAX) * 4096.0).round() as u16;
        let halves = (0..70_000u32).map(|k| (k as f32 + 0.5) / 4096.0);
        let near = (0..200_000u32).map(|k| f32::from_bits(0x3F00_0000 + k * 977) / 3.0);
        let edges = [-1.0, 0.0, f32::NAN, f32::INFINITY, GAIN_MAX, 20.0, 1e-9];
        for g in halves.chain(near).chain(edges) {
            assert_eq!(q12(g), old(g), "{g}");
        }
        for (n, nodes) in [(1280, 32), (800, 32), (640, 16), (1281, 17), (7, 3), (1, 1)] {
            let want: Vec<(u16, u32)> = (0..n)
                .map(|i| {
                    if nodes < 2 || n < 2 {
                        return (0, 0);
                    }
                    let pos = i as f64 * (nodes - 1) as f64 / (n - 1) as f64;
                    let idx = (pos.floor() as usize).min(nodes - 2);
                    let frac = ((pos - idx as f64) * 65536.0).round() as u32;
                    (idx as u16, frac.min(65536))
                })
                .collect();
            assert_eq!(grid_map(n, nodes), want, "{n} {nodes}");
        }
    }
}
