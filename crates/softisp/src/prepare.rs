//! Parameters compiled into the fixed-point tables the row loop uses.

use crate::format::{CfaPattern, Channel, RawFormat};
use crate::params::{Demosaic, IspParams, LensShading, YuvMatrix};
use crate::simd::scalar::WORK_MAX;
use crate::simd::{ToneLut, YuvCoeffs};
use crate::{IspError, StatsConfig};

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
    /// Left shift putting input samples' top bit at bit 15.
    pub shift: u32,
    /// `black[row parity][column parity]`.
    pub black: [[u16; 2]; 2],
    pub gains: GainRows,
    /// Q12 colour matrix.
    pub ccm: Option<[i16; 9]>,
    pub lut: Option<ToneLut>,
    pub yuv: YuvCoeffs,
    pub stats: Option<StatsSetup>,
    /// White balance times digital gain, reported with the statistics.
    pub channel_gains: [f32; 3],
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

fn channel_index(c: Channel) -> usize {
    match c {
        Channel::Red => 0,
        Channel::Green => 1,
        Channel::Blue => 2,
    }
}

fn q12(g: f32) -> u16 {
    (g.clamp(0.0, GAIN_MAX) * 4096.0).round() as u16
}

/// Sample positions of `n` output points spread over `nodes` grid nodes: index and Q16 weight.
fn grid_map(n: usize, nodes: usize) -> Vec<(u16, u32)> {
    (0..n)
        .map(|i| {
            if nodes < 2 || n < 2 {
                return (0, 0);
            }
            let pos = i as f64 * (nodes - 1) as f64 / (n - 1) as f64;
            let idx = (pos.floor() as usize).min(nodes - 2);
            let frac = ((pos - idx as f64) * 65536.0).round() as u32;
            (idx as u16, frac.min(65536))
        })
        .collect()
}

impl Prepared {
    pub fn new(format: &RawFormat, params: &IspParams) -> Result<Self, IspError> {
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
        let ccm = params
            .ccm
            .map(|c| {
                let mut m = [0i16; 9];
                for (k, v) in c.m.iter().flatten().enumerate() {
                    if !v.is_finite() || v.abs() > 4.0 {
                        return Err(IspError::InvalidParams("CCM coefficient outside ±4".into()));
                    }
                    m[k] = (v * 4096.0).round() as i16;
                }
                Ok(m)
            })
            .transpose()?;
        let lut = params
            .tone
            .as_ref()
            .map(|curve| ToneLut::from_curve(|x| curve.eval(x)));
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
            shift: 16 - bits as u32,
            black,
            gains,
            ccm,
            lut,
            yuv,
            stats,
            channel_gains,
        })
    }
}

fn shaded_gains(
    ls: &LensShading,
    format: &RawFormat,
    cell_gain: &dyn Fn(usize, usize) -> f32,
) -> Result<GainRows, IspError> {
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
    let (w, h) = (format.width as usize, format.height as usize);
    let grids = [&ls.r, &ls.g, &ls.b];
    let x_map = grid_map(w, gw);
    // Columns of each parity: their weights of the second grid node, and runs of them between
    // the same two grid nodes (so the inner loops below are plain vector arithmetic).
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
                        let (a, b, g) = (node[i], node[j], gain[p]);
                        for (d, &t) in out[k0..k1].iter_mut().zip(&ts[k0..k1]) {
                            *d = q12(g * (a * (1.0 - t) + b * t));
                        }
                    }
                }
                let mut row = vec![0u16; w];
                for (k, pair) in row.chunks_mut(2).enumerate() {
                    pair[0] = half[0][k];
                    if let Some(odd) = pair.get_mut(1) {
                        *odd = half[1][k];
                    }
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
    /// The gains of image row `y` (`scratch` receives interpolated lens shading rows).
    pub fn row<'a>(&'a self, y: usize, scratch: &'a mut Vec<u16>) -> &'a [u16] {
        match self {
            Self::Flat(rows) => &rows[y & 1],
            Self::Shaded { rows, y_map } => {
                let rows = &rows[y & 1];
                let (i, f) = y_map[y];
                let a = &rows[i as usize];
                let b = &rows[(i as usize + 1).min(rows.len() - 1)];
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
