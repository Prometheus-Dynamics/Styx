//! [`Arithmetic::Half`]'s tables: fp16 black levels and channel gains, lens shading rows, the
//! colour matrix with the demosaic folded in, and the tone curve's segments.

// Feature `fp16` (on with `std`); without it nothing here is made.
#![cfg_attr(not(feature = "fp16"), allow(dead_code, unused_imports))]

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::format::RawFormat;
use crate::params::{Arithmetic, ColorMatrix, Demosaic, IspParams, LensShading, ToneCurve};
use crate::prepare::{Scratch, Spare, grid_map_into, writable};
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
    /// Without feature `fp16` no `HalfPrep` can exist (this field has no value): the fp16
    /// paths still compile, but every match arm on `Arith::Half` is dead and drops out.
    #[cfg(not(feature = "fp16"))]
    never: core::convert::Infallible,
}

/// Lens shading without the channel gains (those change every frame; these only when the
/// grid does).
#[derive(Debug, Default)]
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
        if !cfg!(feature = "fp16") {
            return None;
        }
        let wanted = match params.arithmetic {
            Arithmetic::Auto | Arithmetic::Int | Arithmetic::ApproxTone => false,
            Arithmetic::Half => true,
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

    /// Without feature `fp16` there is nothing to make ([`Self::eligible`] says `None`).
    #[cfg(not(feature = "fp16"))]
    pub fn new(
        _format: &RawFormat,
        _params: &IspParams,
        _tone: HalfTone,
        _channel_gains: [f32; 3],
        _previous: Option<&HalfPrep>,
        _spare: &mut Spare,
        _lsc_tolerance: f32,
    ) -> Self {
        unreachable!("fp16 arithmetic without feature `fp16`")
    }

    #[cfg(feature = "fp16")]
    pub fn new(
        format: &RawFormat,
        params: &IspParams,
        tone: HalfTone,
        channel_gains: [f32; 3],
        previous: Option<&HalfPrep>,
        spare: &mut Spare,
        lsc_tolerance: f32,
    ) -> Self {
        let pattern = format.pattern;
        let full = f64::from((1u32 << format.packing.bit_depth()) - 1);
        let black_cells = params.black_level.map_or([0; 4], |b| b.cells());
        let cell = |x: usize, y: usize| f64::from(black_cells[pattern.cell_at(x, y)]);
        let black = [0, 1].map(|y| [0, 1].map(|x| f16::from_f64(1024.0 + cell(x, y))));
        let gain = [0, 1].map(|y| {
            [0, 1].map(|x| {
                let g = f64::from(channel_gains[pattern.channel_at(x, y).index()]);
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
                    _ => writable(&mut spare.half_lsc, HalfLsc::default, |l| {
                        l.fill(ls, format, &mut spare.scratch)
                    }),
                });
        let mut curve = spare.half_curve.take();
        curve.clone_from(&params.tone);
        Self {
            black,
            gain,
            lsc,
            colour: [0, 1].map(|y| ColourCoeffs::new(&m, RowKind::of(pattern.into(), y))),
            quad: core::array::from_fn(|i| f16::from_f32(m[i / 3][i % 3])),
            tone,
            stats_scale: f16::from_f64(4096.0 * full / (full + 1.0) / half::FULL),
            curve,
        }
    }

    /// The lens shading rows and the curve, for [`Spare::keep`].
    pub fn into_spare(self) -> (Option<Arc<HalfLsc>>, Option<ToneCurve>) {
        (self.lsc, self.curve)
    }
}

/// Whether every node of `b` is within `tolerance` (relative) of `a`'s.
pub(crate) fn close(a: &LensShading, b: &LensShading, tolerance: f32) -> bool {
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
    /// The rows of `ls` written into these (their buffers reused; `s` holds working rows).
    fn fill(&mut self, ls: &LensShading, format: &RawFormat, s: &mut Scratch) {
        let (gw, gh) = (ls.width as usize, ls.height as usize);
        let (w, h) = (format.width as usize, format.height as usize);
        let grids = [&ls.r, &ls.g, &ls.b];
        grid_map_into(&mut s.x_map, w, gw);
        let x_map = &mut s.lanes;
        x_map.clear();
        x_map.extend(s.x_map.iter().map(|&(i, f)| {
            let i = (i as usize).min(gw - 1);
            (i, (i + 1).min(gw - 1), f as f32 / 65536.0)
        }));
        let [cur, next, diff] = &mut s.rows;
        for v in [&mut *cur, &mut *next, &mut *diff] {
            v.resize(w, 0.0);
        }
        for parity in 0..2 {
            let grid = [0, 1].map(|x| grids[format.pattern.channel_at(x, parity).index()]);
            // Gains along grid row `gy` (`x` interpolated between its nodes).
            let along = |gy: usize, out: &mut [f32]| {
                let nodes = grid.map(|g| &g[gy * gw..][..gw]);
                for (o, m) in out.chunks_mut(2).zip(x_map.chunks(2)) {
                    for ((o, &(i, j, t)), node) in o.iter_mut().zip(m).zip(nodes) {
                        *o = node[i] + (node[j] - node[i]) * t;
                    }
                }
            };
            along(0, cur);
            let (a, d) = (&mut self.a[parity], &mut self.d[parity]);
            a.resize_with(gh, Vec::new);
            d.resize_with(gh, Vec::new);
            for row in a.iter_mut().chain(d.iter_mut()) {
                row.resize(w, 0);
            }
            for gy in 0..gh {
                along((gy + 1).min(gh - 1), next);
                for ((v, &c), &n) in diff.iter_mut().zip(cur.iter()).zip(next.iter()) {
                    *v = n - c;
                }
                half::from_f32_row(cur, &mut a[gy]);
                half::from_f32_row(diff, &mut d[gy]);
                core::mem::swap(cur, next);
            }
        }
        grid_map_into(&mut s.x_map, h, gh);
        self.y_map.clear();
        self.y_map.extend(
            s.x_map
                .iter()
                .map(|&(i, f)| (i, f16::from_f64(f64::from(f) / 65536.0))),
        );
        self.grid.clone_from(ls);
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
