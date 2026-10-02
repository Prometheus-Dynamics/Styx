//! [`Arithmetic::Half`]'s tables: fp16 black levels and channel gains, lens shading rows, the
//! colour matrix with the demosaic folded in, and the tone curve's segments.

use std::sync::Arc;

use crate::format::RawFormat;
use crate::params::{Arithmetic, ColorMatrix, Demosaic, IspParams, LensShading, ToneCurve};
use crate::prepare::{channel_index, grid_map};
use crate::simd::half::{ColourCoeffs, HalfTone};
use crate::simd::{RowKind, f16, half};

#[derive(Debug)]
pub(crate) struct HalfPrep {
    /// fp16 of `1024 + black level`, `[row parity][column parity]`.
    pub black: [[u16; 2]; 2],
    /// fp16 channel gain (white balance, digital gain, the range above black stretched to
    /// 0..4080), `[row parity][column parity]`.
    pub gain: [[u16; 2]; 2],
    pub lsc: Option<Arc<HalfLsc>>,
    /// Bilinear colour coefficients for even and odd rows.
    pub colour: [ColourCoeffs; 2],
    /// The colour matrix for quads (fp16, row-major).
    pub quad: [u16; 9],
    pub tone: HalfTone,
    /// Working values to the statistics' 12-bit scale: that of the integer arithmetic, whose
    /// full scale is `4096 full / (full + 1)` (4092 for 10-bit input), so that both report
    /// the same statistics.
    pub stats_scale: u16,
    /// The curve `tone` came from.
    curve: Option<ToneCurve>,
}

/// Lens shading without the channel gains (those change every frame; these only when the
/// grid does).
#[derive(Debug)]
pub(crate) struct HalfLsc {
    /// `a[row parity][grid row]`: the gain at each column on the grid row (fp16).
    pub a: [Vec<Vec<u16>>; 2],
    /// `d[row parity][grid row]`: the next grid row's gain minus this one's (fp16).
    pub d: [Vec<Vec<u16>>; 2],
    /// Per image row: the grid row above and the fp16 weight of the one below.
    pub y_map: Vec<(u16, u16)>,
    /// The grid these rows came from.
    pub grid: LensShading,
}

impl HalfPrep {
    /// The tone curve's segments when `params` run in fp16 for `format` on this CPU (those of
    /// `previous` when its curve is the same: fitting them costs a quarter of a millisecond).
    pub fn eligible(
        format: &RawFormat,
        params: &IspParams,
        previous: Option<&HalfPrep>,
    ) -> Option<HalfTone> {
        let wanted = match params.arithmetic {
            Arithmetic::Int => false,
            Arithmetic::Half => true,
            // Without a colour matrix or a tone curve the integer path does less (no matrix,
            // a plain narrowing): 1.7 instead of 2.3 ms per 1280x800 RGB24 frame on the A76.
            Arithmetic::Auto => half::hardware() && (params.ccm.is_some() || params.tone.is_some()),
        };
        if !wanted || format.packing.bit_depth() > 10 || params.demosaic != Demosaic::Bilinear {
            return None;
        }
        if let Some(p) = previous
            && p.curve == params.tone
        {
            return Some(p.tone.clone());
        }
        match &params.tone {
            Some(curve) => HalfTone::from_curve(|x| curve.eval(x)),
            None => HalfTone::from_curve(|x| x),
        }
    }

    pub fn new(
        format: &RawFormat,
        params: &IspParams,
        tone: HalfTone,
        channel_gains: [f32; 3],
        previous: Option<&HalfPrep>,
        lsc_tolerance: f32,
    ) -> Self {
        let pattern = format.pattern;
        let full = f64::from((1u32 << format.packing.bit_depth()) - 1);
        let black_cells = params.black_level.map_or([0; 4], |b| b.cells());
        let cell = |x: usize, y: usize| f64::from(black_cells[pattern.cell_at(x, y)]);
        let black = [0, 1].map(|y| [0, 1].map(|x| f16::from_f64(1024.0 + cell(x, y))));
        let gain = [0, 1].map(|y| {
            [0, 1].map(|x| {
                let g = f64::from(channel_gains[channel_index(pattern.channel_at(x, y))]);
                f16::from_f64(g * half::FULL / (full - cell(x, y)))
            })
        });
        let m = params.ccm.unwrap_or(ColorMatrix::IDENTITY).m;
        let lsc =
            params
                .lens_shading
                .as_ref()
                .map(|ls| match previous.and_then(|p| p.lsc.as_ref()) {
                    Some(old) if close(&old.grid, ls, lsc_tolerance) => old.clone(),
                    _ => Arc::new(HalfLsc::new(ls, format)),
                });
        Self {
            black,
            gain,
            lsc,
            colour: [0, 1].map(|y| ColourCoeffs::new(&m, RowKind::of(pattern, y))),
            quad: std::array::from_fn(|i| f16::from_f32(m[i / 3][i % 3])),
            tone,
            stats_scale: f16::from_f64(4096.0 * full / (full + 1.0) / half::FULL),
            curve: params.tone.clone(),
        }
    }
}

/// Whether every node of `b` is within `tolerance` (relative) of `a`'s.
fn close(a: &LensShading, b: &LensShading, tolerance: f32) -> bool {
    if (a.width, a.height) != (b.width, b.height) {
        return false;
    }
    [(&a.r, &b.r), (&a.g, &b.g), (&a.b, &b.b)]
        .iter()
        .all(|(x, y)| {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(&p, &q)| (p - q).abs() <= tolerance * p.abs())
        })
}

impl HalfLsc {
    fn new(ls: &LensShading, format: &RawFormat) -> Self {
        let (gw, gh) = (ls.width as usize, ls.height as usize);
        let (w, h) = (format.width as usize, format.height as usize);
        let grids = [&ls.r, &ls.g, &ls.b];
        let x_map: Vec<(usize, usize, f32)> = grid_map(w, gw)
            .into_iter()
            .map(|(i, f)| {
                let i = (i as usize).min(gw - 1);
                (i, (i + 1).min(gw - 1), f as f32 / 65536.0)
            })
            .collect();
        let mut a = [Vec::with_capacity(gh), Vec::with_capacity(gh)];
        let mut d = [Vec::with_capacity(gh), Vec::with_capacity(gh)];
        let (mut cur, mut next, mut diff) = (vec![0f32; w], vec![0f32; w], vec![0f32; w]);
        for parity in 0..2 {
            let grid = [0, 1].map(|x| grids[channel_index(format.pattern.channel_at(x, parity))]);
            // Gains along grid row `gy` (`x` interpolated between its nodes).
            let along = |gy: usize, out: &mut [f32]| {
                let nodes = grid.map(|g| &g[gy * gw..][..gw]);
                for (o, m) in out.chunks_mut(2).zip(x_map.chunks(2)) {
                    for ((o, &(i, j, t)), node) in o.iter_mut().zip(m).zip(nodes) {
                        *o = node[i] + (node[j] - node[i]) * t;
                    }
                }
            };
            along(0, &mut cur);
            for gy in 0..gh {
                along((gy + 1).min(gh - 1), &mut next);
                for ((v, &c), &n) in diff.iter_mut().zip(&cur).zip(&next) {
                    *v = n - c;
                }
                let (mut ra, mut rd) = (vec![0u16; w], vec![0u16; w]);
                half::from_f32_row(&cur, &mut ra);
                half::from_f32_row(&diff, &mut rd);
                a[parity].push(ra);
                d[parity].push(rd);
                std::mem::swap(&mut cur, &mut next);
            }
        }
        let y_map = grid_map(h, gh)
            .into_iter()
            .map(|(i, f)| (i, f16::from_f64(f64::from(f) / 65536.0)))
            .collect();
        Self {
            a,
            d,
            y_map,
            grid: ls.clone(),
        }
    }

    /// The rows for image row `y`.
    pub fn row(&self, y: usize) -> half::LscRow<'_> {
        let (gy, t) = self.y_map[y];
        let p = y & 1;
        half::LscRow {
            a: &self.a[p][gy as usize],
            d: &self.d[p][gy as usize],
            t,
        }
    }
}
