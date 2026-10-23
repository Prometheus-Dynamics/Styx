//! A bounding volume hierarchy over a scene's triangles, and the nearest hit of a ray.

use super::super::geometry::Vec3;
use super::load::Tri;

/// A node: its bounds, and either two children (`left`, `left + 1`... see `build`) or a range
/// of triangles.
#[derive(Clone, Copy, Debug)]
struct Node {
    min: Vec3,
    max: Vec3,
    /// Leaf: the first triangle (in `order`); inner: the first child's index.
    start: u32,
    /// Leaf: the number of triangles; inner: 0.
    count: u32,
    /// Inner: the second child's index.
    right: u32,
}

pub(super) struct Bvh {
    nodes: Vec<Node>,
    order: Vec<u32>,
}

/// The nearest hit: distance along the ray, the triangle, barycentric `u`, `v`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Hit {
    pub(super) t: f64,
    pub(super) tri: u32,
    pub(super) u: f64,
    pub(super) v: f64,
}

const LEAF: usize = 4;

fn bounds(tris: &[Tri], idx: &[u32]) -> (Vec3, Vec3) {
    let mut lo = [f64::MAX; 3];
    let mut hi = [f64::MIN; 3];
    for &i in idx {
        let t = &tris[i as usize];
        for p in [t.v0, add(t.v0, t.e1), add(t.v0, t.e2)] {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
    }
    (lo, hi)
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn centroid(t: &Tri, k: usize) -> f64 {
    t.v0[k] + (t.e1[k] + t.e2[k]) / 3.0
}

impl Bvh {
    /// Splits at the median of the longest axis of the centroids until leaves of a few
    /// triangles.
    pub(super) fn build(tris: &[Tri]) -> Self {
        let mut order: Vec<u32> = (0..tris.len() as u32).collect();
        let mut nodes = Vec::new();
        if tris.is_empty() {
            return Self { nodes, order };
        }
        // (node index, start, end) to split.
        let (lo, hi) = bounds(tris, &order);
        nodes.push(Node {
            min: lo,
            max: hi,
            start: 0,
            count: tris.len() as u32,
            right: 0,
        });
        let mut todo = vec![(0usize, 0usize, tris.len())];
        while let Some((n, start, end)) = todo.pop() {
            if end - start <= LEAF {
                continue;
            }
            let idx = &mut order[start..end];
            let (clo, chi) = idx
                .iter()
                .fold(([f64::MAX; 3], [f64::MIN; 3]), |(lo, hi), &i| {
                    let t = &tris[i as usize];
                    let c = [0, 1, 2].map(|k| centroid(t, k));
                    (
                        [0, 1, 2].map(|k| lo[k].min(c[k])),
                        [0, 1, 2].map(|k| hi[k].max(c[k])),
                    )
                });
            let axis = (0..3)
                .max_by(|&a, &b| (chi[a] - clo[a]).total_cmp(&(chi[b] - clo[b])))
                .unwrap_or(0);
            let mid = (end - start) / 2;
            idx.select_nth_unstable_by(mid, |&a, &b| {
                centroid(&tris[a as usize], axis).total_cmp(&centroid(&tris[b as usize], axis))
            });
            let split = start + mid;
            let left = nodes.len();
            for (s, e) in [(start, split), (split, end)] {
                let (lo, hi) = bounds(tris, &order[s..e]);
                nodes.push(Node {
                    min: lo,
                    max: hi,
                    start: s as u32,
                    count: (e - s) as u32,
                    right: 0,
                });
            }
            nodes[n] = Node {
                start: left as u32,
                count: 0,
                right: left as u32 + 1,
                ..nodes[n]
            };
            todo.push((left, start, split));
            todo.push((left + 1, split, end));
        }
        Self { nodes, order }
    }

    /// The nearest triangle `d` from `o` meets (both sides), beyond `t_min`, before `t_max`.
    pub(super) fn hit(
        &self,
        tris: &[Tri],
        o: Vec3,
        d: Vec3,
        t_min: f64,
        t_max: f64,
    ) -> Option<Hit> {
        if self.nodes.is_empty() {
            return None;
        }
        let inv = d.map(|c| {
            if c.abs() < 1e-30 {
                1e30f64.copysign(c)
            } else {
                1.0 / c
            }
        });
        let mut best: Option<Hit> = None;
        let mut limit = t_max;
        let mut stack = [0u32; 64];
        let mut sp = 1;
        while sp > 0 {
            sp -= 1;
            let node = &self.nodes[stack[sp] as usize];
            if !slab(node, o, inv, t_min, limit) {
                continue;
            }
            if node.count > 0 {
                for &i in &self.order[node.start as usize..(node.start + node.count) as usize] {
                    if let Some(h) = triangle(&tris[i as usize], o, d)
                        && h.0 > t_min
                        && h.0 < limit
                    {
                        limit = h.0;
                        best = Some(Hit {
                            t: h.0,
                            tri: i,
                            u: h.1,
                            v: h.2,
                        });
                    }
                }
            } else if sp + 2 <= stack.len() {
                stack[sp] = node.start;
                stack[sp + 1] = node.right;
                sp += 2;
            }
        }
        best
    }
}

fn slab(n: &Node, o: Vec3, inv: Vec3, t_min: f64, t_max: f64) -> bool {
    let (mut lo, mut hi) = (t_min, t_max);
    for k in 0..3 {
        let a = (n.min[k] - o[k]) * inv[k];
        let b = (n.max[k] - o[k]) * inv[k];
        lo = lo.max(a.min(b));
        hi = hi.min(a.max(b));
        if lo > hi {
            return false;
        }
    }
    true
}

/// Möller–Trumbore: the distance and barycentric coordinates of the hit, if any.
fn triangle(t: &Tri, o: Vec3, d: Vec3) -> Option<(f64, f64, f64)> {
    let p = cross(d, t.e2);
    let det = dot(t.e1, p);
    if det.abs() < 1e-14 {
        return None;
    }
    let inv = 1.0 / det;
    let s = [o[0] - t.v0[0], o[1] - t.v0[1], o[2] - t.v0[2]];
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(s, t.e1);
    let v = dot(d, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    Some((dot(t.e2, q) * inv, u, v))
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
