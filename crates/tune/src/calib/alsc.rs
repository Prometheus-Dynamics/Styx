//! Lens shading from flat fields, the method of Raspberry Pi's `ctt`: the image split into the
//! table's cells, each cell's channel means (black level removed); the luminance table is the
//! green fall-off `max(G) / G`, the colour tables `G / R` and `G / B`, each smoothed ([1 2 1]
//! across neighbouring cells) and normalised to a smallest gain of 1; tables of the same temperature averaged; the luminance table averaged
//! over all; the adaptive algorithm's `sigma` from how much adjacent temperatures' colour
//! tables differ from cell to cell.

use crate::raw::{B, GB, GR, Planes, R};

/// Tables at one temperature.
#[derive(Clone, Debug, PartialEq)]
pub struct AlscTable {
    /// Colour temperature.
    pub ct: f64,
    /// Red gains `G / R`, normalised to a minimum of 1.
    pub cr: Vec<f64>,
    /// Blue gains `G / B`, normalised.
    pub cb: Vec<f64>,
    /// Luminance gains `max(G) / G`.
    pub lum: Vec<f64>,
    /// Flats averaged into it.
    pub shots: usize,
}

/// The lens shading calibration.
#[derive(Clone, Debug, PartialEq)]
pub struct AlscResult {
    /// Cells across and down.
    pub grid: (u32, u32),
    /// In increasing temperature.
    pub tables: Vec<AlscTable>,
    /// Luminance table, averaged over every flat.
    pub luminance: Vec<f64>,
    /// Adaptive ALSC's colour similarity scales.
    pub sigma_cr: f64,
    /// Blue.
    pub sigma_cb: f64,
    /// Problems with the flats (clipping, too dark, not flat).
    pub warnings: Vec<String>,
}

/// A [1 2 1] smoothing in each direction, edges extended linearly (a linear gradient comes
/// through unchanged): takes the noise out of each cell before the tables are normalised by
/// their smallest cell, which would otherwise pick the cell with the most negative noise.
fn smooth(v: &[f64], grid: (u32, u32)) -> Vec<f64> {
    let (w, h) = (grid.0 as usize, grid.1 as usize);
    let pass = |v: &[f64], n: usize, step: usize, count: usize, stride: usize| -> Vec<f64> {
        let mut out = v.to_vec();
        if n < 3 {
            return out;
        }
        for k in 0..count {
            let at = |i: usize| v[k * stride + i * step];
            for i in 0..n {
                let prev = if i == 0 {
                    2.0 * at(0) - at(1)
                } else {
                    at(i - 1)
                };
                let next = if i + 1 == n {
                    2.0 * at(n - 1) - at(n - 2)
                } else {
                    at(i + 1)
                };
                out[k * stride + i * step] = (prev + 2.0 * at(i) + next) / 4.0;
            }
        }
        out
    };
    let rows = pass(v, w, 1, h, w);
    pass(&rows, h, w, w, 1)
}

fn normalise(v: Vec<f64>) -> Vec<f64> {
    let min = v.iter().copied().fold(f64::INFINITY, f64::min).max(1e-12);
    v.into_iter().map(|x| x / min).collect()
}

/// Per-cell channel means over the planes.
pub fn cell_means(p: &Planes, grid: (u32, u32)) -> Vec<[f64; 4]> {
    let (gw, gh) = (grid.0 as usize, grid.1 as usize);
    let mut out = Vec::with_capacity(gw * gh);
    for j in 0..gh {
        for i in 0..gw {
            let (x0, x1) = (i * p.width / gw, (i + 1) * p.width / gw);
            let (y0, y1) = (j * p.height / gh, (j + 1) * p.height / gh);
            out.push(
                p.region(x0, y0, x1.max(x0 + 1), y1.max(y0 + 1))
                    .unwrap_or([0.0; 4]),
            );
        }
    }
    out
}

/// Calibrate from flats `(ct, planes with the black level removed)`.
pub fn calibrate(flats: &[(f64, &Planes)], grid: (u32, u32)) -> Option<AlscResult> {
    if flats.is_empty() {
        return None;
    }
    let mut warnings = Vec::new();
    let mut singles: Vec<AlscTable> = Vec::new();
    for (ct, p) in flats {
        let cells = cell_means(p, grid);
        let g: Vec<f64> = cells
            .iter()
            .map(|c| ((c[GR] + c[GB]) / 2.0).max(1e-6))
            .collect();
        let peak = cells.iter().flatten().copied().fold(0.0, f64::max);
        let gmax = g.iter().copied().fold(0.0, f64::max);
        if peak > 0.93 {
            warnings.push(format!(
                "flat at {ct} K: brightest cell at {peak:.3} of full scale (clipping?)"
            ));
        }
        if gmax < 0.15 {
            warnings.push(format!(
                "flat at {ct} K: centre at {gmax:.3} of full scale (aim for 0.5-0.8)"
            ));
        }
        let table = |f: &dyn Fn(&[f64; 4], f64) -> f64| -> Vec<f64> {
            smooth(
                &cells
                    .iter()
                    .zip(&g)
                    .map(|(c, g)| f(c, *g))
                    .collect::<Vec<_>>(),
                grid,
            )
        };
        singles.push(AlscTable {
            ct: *ct,
            cr: normalise(table(&|c, g| g / c[R].max(1e-6))),
            cb: normalise(table(&|c, g| g / c[B].max(1e-6))),
            lum: normalise(table(&|_, g| gmax / g)),
            shots: 1,
        });
    }
    // Average tables of the same temperature (within 1 K).
    singles.sort_by(|a, b| a.ct.total_cmp(&b.ct));
    let mut tables: Vec<AlscTable> = Vec::new();
    for t in singles {
        match tables.last_mut() {
            Some(last) if (last.ct - t.ct).abs() < 1.0 => {
                let n = last.shots as f64;
                let mix = |a: &mut Vec<f64>, b: &[f64]| {
                    a.iter_mut()
                        .zip(b)
                        .for_each(|(x, y)| *x = (*x * n + y) / (n + 1.0));
                };
                mix(&mut last.cr, &t.cr);
                mix(&mut last.cb, &t.cb);
                mix(&mut last.lum, &t.lum);
                last.shots += 1;
            }
            _ => tables.push(t),
        }
    }
    let total: f64 = tables.iter().map(|t| t.shots as f64).sum();
    let cells = tables[0].lum.len();
    let luminance = normalise(
        (0..cells)
            .map(|i| {
                tables
                    .iter()
                    .map(|t| t.lum[i] * t.shots as f64)
                    .sum::<f64>()
                    / total
            })
            .collect(),
    );
    let (sigma_cr, sigma_cb) = if tables.len() < 2 {
        (0.005, 0.005)
    } else {
        let worst = |pick: fn(&AlscTable) -> &Vec<f64>| {
            tables
                .windows(2)
                .map(|w| sigma(pick(&w[0]), pick(&w[1]), grid))
                .fold(0.0, f64::max)
        };
        (worst(|t| &t.cr), worst(|t| &t.cb))
    };
    Some(AlscResult {
        grid,
        tables,
        luminance,
        sigma_cr,
        sigma_cb,
        warnings,
    })
}

/// Mean absolute difference between each interior cell of `a / b` and its four neighbours.
fn sigma(a: &[f64], b: &[f64], grid: (u32, u32)) -> f64 {
    let (w, h) = (grid.0 as usize, grid.1 as usize);
    let mut r: Vec<f64> = a.iter().zip(b).map(|(x, y)| x / y).collect();
    if r.iter().sum::<f64>() / (r.len() as f64) < 1.0 {
        r.iter_mut().for_each(|v| *v = 1.0 / *v);
    }
    let mut diffs = Vec::new();
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let c = r[y * w + x];
            let d = (c - r[(y - 1) * w + x]).abs()
                + (c - r[(y + 1) * w + x]).abs()
                + (c - r[y * w + x - 1]).abs()
                + (c - r[y * w + x + 1]).abs();
            diffs.push(d / 4.0);
        }
    }
    if diffs.is_empty() {
        0.005
    } else {
        (diffs.iter().sum::<f64>() / diffs.len() as f64 * 1e5).round() / 1e5
    }
}

impl AlscResult {
    /// Tables at a temperature (linear in temperature between calibrations, the nearest
    /// outside): `(cr, cb)`.
    fn colour_at(&self, ct: f64) -> (Vec<f64>, Vec<f64>) {
        let t = &self.tables;
        let i = t.iter().position(|e| e.ct >= ct);
        match i {
            Some(0) => (t[0].cr.clone(), t[0].cb.clone()),
            None => (t[t.len() - 1].cr.clone(), t[t.len() - 1].cb.clone()),
            Some(i) => {
                let (a, b) = (&t[i - 1], &t[i]);
                let k = (ct - a.ct) / (b.ct - a.ct);
                let mix =
                    |p: &[f64], q: &[f64]| p.iter().zip(q).map(|(x, y)| x + (y - x) * k).collect();
                (mix(&a.cr, &b.cr), mix(&a.cb, &b.cb))
            }
        }
    }

    /// Flat-field correction for planes of `width × height` quads at a temperature: per-quad
    /// gains `[R, Gr, Gb, B]` (full luminance correction, colour tables at `ct`), bilinear
    /// between cell centres.
    pub fn correction(&self, ct: f64, width: usize, height: usize) -> Correction {
        let (cr, cb) = self.colour_at(ct);
        Correction {
            grid: self.grid,
            width,
            height,
            tables: [
                cr.iter().zip(&self.luminance).map(|(c, l)| c * l).collect(),
                self.luminance.clone(),
                cb.iter().zip(&self.luminance).map(|(c, l)| c * l).collect(),
            ],
        }
    }
}

/// Lens shading correction gains over the planes.
#[derive(Clone, Debug)]
pub struct Correction {
    grid: (u32, u32),
    width: usize,
    height: usize,
    /// R, G, B tables.
    tables: [Vec<f64>; 3],
}

impl Correction {
    /// Gains `[R, Gr, Gb, B]` at plane coordinates.
    pub fn at(&self, x: f64, y: f64) -> [f64; 4] {
        let (gw, gh) = (self.grid.0 as usize, self.grid.1 as usize);
        // Cell centres at (i + 0.5) × width / gw - 0.5.
        let fx = ((x + 0.5) * gw as f64 / self.width as f64 - 0.5).clamp(0.0, (gw - 1) as f64);
        let fy = ((y + 0.5) * gh as f64 / self.height as f64 - 0.5).clamp(0.0, (gh - 1) as f64);
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(gw - 1), (y0 + 1).min(gh - 1));
        let (tx, ty) = (fx - x0 as f64, fy - y0 as f64);
        let get = |t: &Vec<f64>| {
            let a = t[y0 * gw + x0] * (1.0 - tx) + t[y0 * gw + x1] * tx;
            let b = t[y1 * gw + x0] * (1.0 - tx) + t[y1 * gw + x1] * tx;
            a * (1.0 - ty) + b * ty
        };
        let (r, g, b) = (
            get(&self.tables[0]),
            get(&self.tables[1]),
            get(&self.tables[2]),
        );
        [r, g, g, b]
    }

    /// Apply to a set of patch means at their centres.
    pub fn apply(&self, centre: [f64; 2], mean: [f64; 4]) -> [f64; 4] {
        let g = self.at(centre[0], centre[1]);
        std::array::from_fn(|c| mean[c] * g[c])
    }
}
