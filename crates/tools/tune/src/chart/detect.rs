//! Finding the ColorChecker automatically.
//!
//! The patches are the uniform squares of an image: a pixel is "uniform" when the colour
//! variance in a small window around it is small next to its brightness. Connected uniform
//! regions that are roughly square are candidate patches; the chart is the largest set of them
//! on a regular lattice (the dominant direction and spacing of neighbouring candidates), at
//! most 6 × 4 of it. A homography from chart coordinates to the members' centroids places all
//! 24 patches, including those that were not found (the black patch often merges with the
//! chart's frame). Which corner is dark skin is decided by the colours: of the four ways the
//! lattice can map onto the chart, the one whose neutral row is neutral and whose patches best
//! match the reference colours after white balancing on that row. Window sizes from 3 to 17
//! pixels are tried, so the patch size does not need to be known.
//!
//! The chart should be roughly frontal (perspective is fitted, but the neighbour search
//! assumes a near-regular lattice), fill a fair part of the frame (patches of 8 pixels or more
//! after downscaling to ≤ 800 pixels across), and be lit evenly.

use super::{Chart, Patch};
use crate::colour::{WB_GREYS, macbeth_linear};
use crate::linalg::{Mat3, homography, mul};
use crate::raw::Planes;

/// A chart found in an image.
#[derive(Clone, Debug)]
pub struct Detection {
    /// Where it is.
    pub chart: Chart,
    /// Patches found as uniform regions (the rest were placed by the fit).
    pub found: usize,
    /// Mismatch to the reference colours after white balance (lower is better; the
    /// orientation was chosen by it).
    pub cost: f64,
    /// Neighbourhood size that found it, pixels of the downscaled image.
    pub window: usize,
}

#[derive(Clone, Copy, Debug)]
struct Comp {
    c: [f64; 2],
    s: f64,
}

/// Box mean over a `(2r+1)²` window, clamped at the edges, from an integral image.
fn box_mean(v: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut ii = vec![0f64; (w + 1) * (h + 1)];
    for y in 0..h {
        let mut row = 0.0;
        for x in 0..w {
            row += f64::from(v[y * w + x]);
            ii[(y + 1) * (w + 1) + x + 1] = ii[y * (w + 1) + x + 1] + row;
        }
    }
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        let (y0, y1) = (y.saturating_sub(r), (y + r + 1).min(h));
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(r), (x + r + 1).min(w));
            let s = ii[y1 * (w + 1) + x1] - ii[y0 * (w + 1) + x1] - ii[y1 * (w + 1) + x0]
                + ii[y0 * (w + 1) + x0];
            out[y * w + x] = (s / ((x1 - x0) * (y1 - y0)) as f64) as f32;
        }
    }
    out
}

/// Candidate patches: connected uniform regions that are square enough.
fn components(img: &[Vec<f32>; 3], w: usize, h: usize, r: usize) -> Vec<Comp> {
    let mean: Vec<Vec<f32>> = img.iter().map(|c| box_mean(c, w, h, r)).collect();
    let sq: Vec<Vec<f32>> = img
        .iter()
        .map(|c| box_mean(&c.iter().map(|v| v * v).collect::<Vec<_>>(), w, h, r))
        .collect();
    let mask: Vec<bool> = (0..w * h)
        .map(|i| {
            let var: f32 = (0..3)
                .map(|c| (sq[c][i] - mean[c][i] * mean[c][i]).max(0.0))
                .sum();
            let y = (mean[0][i] + 2.0 * mean[1][i] + mean[2][i]) / 4.0;
            y > 0.004 && var.sqrt() / (y + 0.02) < 0.1
        })
        .collect();
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !mask[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        let (mut n, mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            let (fx, fy) = (x as f64, y as f64);
            n += 1.0;
            sx += fx;
            sy += fy;
            sxx += fx * fx;
            syy += fy * fy;
            sxy += fx * fy;
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            let mut push = |j: usize| {
                if mask[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 {
                push(i - 1);
            }
            if x + 1 < w {
                push(i + 1);
            }
            if y > 0 {
                push(i - w);
            }
            if y + 1 < h {
                push(i + w);
            }
        }
        let bbox = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f64;
        if n < 6.0 || n > (w * h) as f64 / 20.0 || n / bbox < 0.4 {
            continue;
        }
        let (mx, my) = (sx / n, sy / n);
        let (vxx, vyy, vxy) = (sxx / n - mx * mx, syy / n - my * my, sxy / n - mx * my);
        let tr = vxx + vyy;
        let det = vxx * vyy - vxy * vxy;
        let disc = (tr * tr / 4.0 - det).max(0.0).sqrt();
        let (l1, l2) = (tr / 2.0 + disc, (tr / 2.0 - disc).max(1e-9));
        if l1 / l2 > 4.0 {
            continue;
        }
        out.push(Comp {
            c: [mx, my],
            s: n.sqrt(),
        });
    }
    out
}

fn similar(a: &Comp, b: &Comp, k: f64) -> bool {
    let r = a.s / b.s;
    r < k && r > 1.0 / k
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// Angle difference modulo 90°, in [0, 45].
fn diff90(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(90.0);
    d.min(90.0 - d)
}

/// A lattice member: component index and lattice coordinates.
type Member = (usize, i32, i32);

/// The lattice: its members, and the pitch.
fn lattice(comps: &[Comp]) -> Option<(Vec<Member>, f64)> {
    let n = comps.len();
    if n < 8 {
        return None;
    }
    // Directions of the nearest similar neighbours, modulo 90°.
    let mut near: Vec<Vec<(f64, usize)>> = vec![Vec::new(); n];
    let mut hist = [0f64; 90];
    for i in 0..n {
        let mut v: Vec<(f64, usize)> = (0..n)
            .filter(|&j| j != i && similar(&comps[i], &comps[j], 2.0))
            .map(|j| (dist(comps[i].c, comps[j].c), j))
            .filter(|(d, _)| *d < 4.0 * comps[i].s)
            .collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v.truncate(4);
        for &(_, j) in &v {
            let d = [comps[j].c[0] - comps[i].c[0], comps[j].c[1] - comps[i].c[1]];
            let a = d[1].atan2(d[0]).to_degrees().rem_euclid(90.0);
            hist[a as usize % 90] += 1.0;
        }
        near[i] = v;
    }
    let smooth = |k: usize| (0..7).map(|o| hist[(k + 90 + o - 3) % 90]).sum::<f64>();
    let peak = (0..90).max_by(|&a, &b| smooth(a).total_cmp(&smooth(b)))? as f64 + 0.5;
    // Refine the angle and find the pitch: the nearest aligned neighbour of each candidate.
    let (mut ssin, mut scos) = (0.0, 0.0);
    let mut pitches = Vec::new();
    for (i, v) in near.iter().enumerate() {
        let mut best = f64::INFINITY;
        for &(d, j) in v {
            let dv = [comps[j].c[0] - comps[i].c[0], comps[j].c[1] - comps[i].c[1]];
            let a = dv[1].atan2(dv[0]).to_degrees().rem_euclid(90.0);
            if diff90(a, peak) < 8.0 {
                let t = (a * 4.0).to_radians();
                ssin += t.sin();
                scos += t.cos();
                best = best.min(d);
            }
        }
        if best.is_finite() {
            pitches.push(best);
        }
    }
    if pitches.len() < 8 {
        return None;
    }
    let theta = (ssin.atan2(scos) / 4.0).rem_euclid(std::f64::consts::FRAC_PI_2);
    pitches.sort_by(f64::total_cmp);
    let p = pitches[pitches.len() / 2];
    let u = [p * theta.cos(), p * theta.sin()];
    let v = [-u[1], u[0]];
    // Grow clusters over lattice steps.
    let mut coord: Vec<Option<(i32, i32)>> = vec![None; n];
    let mut best: Vec<(usize, i32, i32)> = Vec::new();
    for start in 0..n {
        if coord[start].is_some() {
            continue;
        }
        coord[start] = Some((0, 0));
        let mut members = vec![(start, 0, 0)];
        let mut k = 0;
        while k < members.len() {
            let (i, a, b) = members[k];
            k += 1;
            for j in 0..n {
                if coord[j].is_some() || !similar(&comps[i], &comps[j], 1.7) {
                    continue;
                }
                let d = [comps[j].c[0] - comps[i].c[0], comps[j].c[1] - comps[i].c[1]];
                for (step, da, db) in [(u, 1, 0), (v, 0, 1)] {
                    for sign in [1.0, -1.0] {
                        let e = [d[0] - sign * step[0], d[1] - sign * step[1]];
                        if (e[0] * e[0] + e[1] * e[1]).sqrt() < 0.3 * p && coord[j].is_none() {
                            let s = sign as i32;
                            coord[j] = Some((a + s * da, b + s * db));
                            members.push((j, a + s * da, b + s * db));
                        }
                    }
                }
            }
        }
        if members.len() > best.len() {
            best = members;
        }
    }
    (best.len() >= 10).then_some((best, p))
}

/// The orientation cost of 24 patch values: their mismatch to the reference after white
/// balance and exposure from the neutral row; infinite when that row is not neutral and
/// ordered.
pub(crate) fn orientation_cost(p: &[Patch; 24]) -> f64 {
    let rgb: Vec<[f64; 3]> = p.iter().map(Patch::rgb).collect();
    let sum = |c: usize| WB_GREYS.map(|i| rgb[i][c]).sum::<f64>();
    let (r, g, b) = (sum(0), sum(1), sum(2));
    if r <= 0.0 || g <= 0.0 || b <= 0.0 {
        return f64::INFINITY;
    }
    let wb = [g / r, 1.0, g / b];
    let reference = macbeth_linear();
    let k = WB_GREYS.map(|i| reference[i][1]).sum::<f64>() / g;
    let y = |i: usize| rgb[i][0] * wb[0] + 2.0 * rgb[i][1] + rgb[i][2] * wb[2];
    let ordered = (19..23).all(|i| y(i) > y(i + 1)) && y(18) >= 0.95 * y(19);
    let neutral = WB_GREYS.clone().all(|i| {
        let g = rgb[i][1].max(1e-9);
        (rgb[i][0] * wb[0] / g).ln().abs() < 0.25 && (rgb[i][2] * wb[2] / g).ln().abs() < 0.25
    });
    if !ordered || !neutral {
        return f64::INFINITY;
    }
    let mut cost = 0.0;
    for (i, ref_rgb) in reference.iter().enumerate() {
        for c in 0..3 {
            let cam = (rgb[i][c] * wb[c] * k).max(0.0) + 0.005;
            cost += (cam.ln() - (ref_rgb[c] + 0.005).ln()).powi(2);
        }
    }
    cost / 72.0
}

/// Find the chart in `planes` (black level removed).
pub fn detect(planes: &Planes) -> Option<Detection> {
    let f = (planes.width as f64 / 800.0).ceil().max(1.0) as usize;
    let (w, h) = (planes.width / f, planes.height / f);
    let mut img: [Vec<f32>; 3] = std::array::from_fn(|_| vec![0f32; w * h]);
    for y in 0..h * f {
        for x in 0..w * f {
            let i = (y / f) * w + x / f;
            let g = (planes.at(1, x, y) + planes.at(2, x, y)) / 2.0;
            img[0][i] += planes.at(0, x, y);
            img[1][i] += g;
            img[2][i] += planes.at(3, x, y);
        }
    }
    let mut luma: Vec<f32> = (0..w * h)
        .map(|i| (img[0][i] + 2.0 * img[1][i] + img[2][i]) / 4.0)
        .collect();
    let top = {
        let k = (luma.len() as f64 * 0.99) as usize;
        let (_, v, _) = luma.select_nth_unstable_by(k.min(w * h - 1), f32::total_cmp);
        v.max(1e-9)
    };
    for c in &mut img {
        c.iter_mut().for_each(|v| *v /= top);
        *c = box_mean(c, w, h, 1);
    }
    // Downscaled coordinates → plane coordinates.
    let fs = f as f64;
    let to_planes: Mat3 = [
        fs,
        0.0,
        (fs - 1.0) / 2.0,
        0.0,
        fs,
        (fs - 1.0) / 2.0,
        0.0,
        0.0,
        1.0,
    ];
    let mut best: Option<Detection> = None;
    for r in [1usize, 2, 3, 5, 8] {
        if 2 * r + 1 > w.min(h) / 8 {
            break;
        }
        let comps = components(&img, w, h, r);
        let Some((members, pitch)) = lattice(&comps) else {
            continue;
        };
        if let Some(d) = place(planes, &comps, &members, pitch, &to_planes, 2 * r + 1) {
            let better = best.as_ref().is_none_or(|b| {
                (d.found, -d.cost) > (b.found, -b.cost) || (d.found == b.found && d.cost < b.cost)
            });
            if better {
                best = Some(d);
            }
        }
    }
    best
}

/// The chart from lattice members: the best 6×4 window and orientation.
fn place(
    planes: &Planes,
    comps: &[Comp],
    members: &[(usize, i32, i32)],
    pitch: f64,
    to_planes: &Mat3,
    window: usize,
) -> Option<Detection> {
    let (amin, amax) = members.iter().fold((i32::MAX, i32::MIN), |(lo, hi), m| {
        (lo.min(m.1), hi.max(m.1))
    });
    let (bmin, bmax) = members.iter().fold((i32::MAX, i32::MIN), |(lo, hi), m| {
        (lo.min(m.2), hi.max(m.2))
    });
    let mut best: Option<Detection> = None;
    for (wa, wb) in [(6, 4), (4, 6)] {
        for a0 in (amax - wa + 1).min(amin)..=amin.max(amax - wa + 1) {
            for b0 in (bmax - wb + 1).min(bmin)..=bmin.max(bmax - wb + 1) {
                let inside: Vec<(usize, i32, i32)> = members
                    .iter()
                    .filter(|m| (a0..a0 + wa).contains(&m.1) && (b0..b0 + wb).contains(&m.2))
                    .map(|&(i, a, b)| (i, a - a0, b - b0))
                    .collect();
                if inside.len() < 10 {
                    continue;
                }
                // Chart (column, row) of each member, for the four orientations.
                for variant in 0..4 {
                    let map = |la: i32, lb: i32| {
                        let (c, r) = if wa == 6 { (la, lb) } else { (lb, la) };
                        match variant {
                            0 => (c, r),
                            1 => (5 - c, 3 - r),
                            2 => (5 - c, r),
                            _ => (c, 3 - r),
                        }
                    };
                    let src: Vec<[f64; 2]> = inside
                        .iter()
                        .map(|&(_, a, b)| {
                            let (c, r) = map(a, b);
                            [f64::from(c), f64::from(r)]
                        })
                        .collect();
                    let dst: Vec<[f64; 2]> = inside.iter().map(|&(i, _, _)| comps[i].c).collect();
                    let Some(hd) = homography(&src, &dst) else {
                        continue;
                    };
                    // Pick up candidates the lattice walk missed, then refit.
                    let (mut src2, mut dst2) = (Vec::new(), Vec::new());
                    for i in 0..24 {
                        let p = crate::linalg::apply(&hd, [(i % 6) as f64, (i / 6) as f64]);
                        if let Some(c) = comps.iter().find(|c| dist(c.c, p) < 0.25 * pitch) {
                            src2.push([(i % 6) as f64, (i / 6) as f64]);
                            dst2.push(c.c);
                        }
                    }
                    let hd = if src2.len() >= inside.len() {
                        homography(&src2, &dst2).unwrap_or(hd)
                    } else {
                        hd
                    };
                    let chart = Chart {
                        h: mul(to_planes, &hd),
                        manual: false,
                    };
                    let cost = orientation_cost(&chart.sample(planes, None));
                    if !cost.is_finite() {
                        continue;
                    }
                    let found = src2.len().max(inside.len());
                    if best.as_ref().is_none_or(|b| {
                        (found, -cost) > (b.found, -b.cost) || (found == b.found && cost < b.cost)
                    }) {
                        best = Some(Detection {
                            chart,
                            found,
                            cost,
                            window,
                        });
                    }
                }
            }
        }
    }
    best
}
