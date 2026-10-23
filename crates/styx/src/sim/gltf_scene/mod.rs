//! `scene=gltf`: a glTF scene (`.gltf` or `.glb`) seen by a simulated camera, the same file
//! Styx's Bevy simulation backend loads. The application builds the scene (geometry,
//! textures, a camera node with an optional animation, `truth:` points); Styx renders it on
//! the CPU through the camera's real lens and reports each frame's ground truth.
//!
//! - **Rendering**: each pixel's ray from the lens model (OpenCV's fisheye or pinhole, from
//!   the spec's keys; else the glTF camera's vertical field of view, a pinhole without
//!   distortion) meets the nearest triangle (a bounding volume hierarchy); the light is the
//!   material's base colour (factor times texture, sRGB textures decoded) times the scene's
//!   brightness, unlit. A ray that meets nothing sees `background`.
//! - **The camera**: the glTF camera node (or the node `camera=<name>`) with its ancestors'
//!   transforms; their translation and rotation animations move it (looped), and a moving
//!   camera blurs over each frame's exposure. glTF cameras look along their -Z with +Y up.
//! - **Ground truth**: the camera's pose and each `truth:<name>` node's pixel when it is in
//!   the frame and not hidden behind the scene (points stay at their rest positions).

mod bvh;
mod load;

use std::path::Path;
use std::sync::{Arc, Mutex};

use styx_native::sim::{FrameContext, GroundTruth, SceneFrame, SimScene};

use self::bvh::Bvh;
use self::load::{Loaded, Material, NodeTransform, Texture, Track, Wrap, compose_all, split, trs};
use super::geometry::{Lens, Mat3, Model, Vec3, mul, mul_t, sub, to_quaternion};

/// Light per second at gain 1 on a surface of reflectance 1 at `light=1` (a white of 0.8 at
/// 80% of full scale at 10 ms, as the chart's).
const LIGHT: f64 = 100.0;

/// The rays of a mode's pixels, and what they were made for (size, field bits).
type Rays = ((u32, u32, [u32; 4]), Arc<Vec<Vec3>>);
/// A still camera's frame, and what it was made for.
type Still = ((u32, u32, [u32; 4]), Arc<SceneFrame>, GroundTruth);

pub(crate) struct GltfScene {
    scene: Loaded,
    bvh: Bvh,
    /// The lens from the spec, if given.
    lens: Option<Lens>,
    light: f64,
    background: f32,
    antialias: Option<bool>,
    blur: bool,
    blur_samples: usize,
    threads: usize,
    rays: Mutex<Option<Rays>>,
    still: Mutex<Option<Still>>,
}

fn parse<T: std::str::FromStr>(k: &str, v: &str) -> Result<T, String> {
    v.parse().map_err(|_| format!("{k}={v}: a number"))
}

impl GltfScene {
    /// From a spec's parameters (see `docs/simulation.md#gltf-scenes`).
    pub(crate) fn from_params(params: &[(String, String)]) -> Result<Self, String> {
        const KEYS: [&str; 23] = [
            "path",
            "camera",
            "lens",
            "lens_width",
            "lens_height",
            "fx",
            "fy",
            "cx",
            "cy",
            "k1",
            "k2",
            "k3",
            "k4",
            "k5",
            "k6",
            "p1",
            "p2",
            "light",
            "background",
            "antialias",
            "blur",
            "blur_samples",
            "threads",
        ];
        if let Some((k, _)) = params.iter().find(|(k, _)| !KEYS.contains(&k.as_str())) {
            return Err(format!("unknown key {k}"));
        }
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let path = get("path").ok_or("path=<scene .gltf or .glb>")?;
        let scene = load::load(Path::new(path), get("camera"))?;
        let num = |k: &str| get(k).map(|v| parse::<f64>(k, v)).transpose();
        let lens = match num("fx")? {
            None => None,
            Some(fx) => {
                let need = |k: &str| num(k)?.ok_or(format!("{k} with fx"));
                let k = |n: &str| num(n).map(|v| v.unwrap_or(0.0));
                let model = match get("lens").unwrap_or("pinhole") {
                    "fisheye" => Model::Fisheye([k("k1")?, k("k2")?, k("k3")?, k("k4")?]),
                    "pinhole" => Model::Pinhole {
                        k: [k("k1")?, k("k2")?, k("k3")?, k("k4")?, k("k5")?, k("k6")?],
                        p: [k("p1")?, k("p2")?],
                    },
                    other => return Err(format!("lens={other}: fisheye or pinhole")),
                };
                Some(Lens {
                    width: need("lens_width")? as u32,
                    height: need("lens_height")? as u32,
                    fx,
                    fy: num("fy")?.unwrap_or(fx),
                    cx: need("cx")?,
                    cy: need("cy")?,
                    model,
                })
            }
        };
        if lens.is_none() && scene.yfov.is_none() {
            return Err("no lens: fx, fy, cx, cy, lens_width, lens_height (and lens, k1.., p1, p2), or a perspective glTF camera".into());
        }
        let on = |k: &str| get(k).map(|v| !matches!(v, "off" | "0" | "false"));
        let bvh = Bvh::build(&scene.tris);
        Ok(Self {
            scene,
            bvh,
            lens,
            light: num("light")?.unwrap_or(1.0),
            background: num("background")?.unwrap_or(0.3) as f32,
            antialias: on("antialias"),
            blur: on("blur").unwrap_or(true),
            blur_samples: num("blur_samples")?.map_or(4, |n| n.clamp(1.0, 16.0) as usize),
            threads: num("threads")?.map_or_else(
                || std::thread::available_parallelism().map_or(2, |n| n.get().min(8)),
                |n| n.clamp(1.0, 64.0) as usize,
            ),
            rays: Mutex::new(None),
            still: Mutex::new(None),
        })
    }

    fn moves(&self) -> bool {
        self.scene.duration > 0.0
            && self
                .scene
                .camera
                .iter()
                .any(|n| n.translation_track.is_some() || n.rotation_track.is_some())
    }

    /// The lens for a mode: the spec's, else a pinhole of the glTF camera's vertical field of
    /// view over the whole pixel array.
    fn lens(&self, ctx: &FrameContext) -> Lens {
        if let Some(l) = self.lens {
            return l;
        }
        let f = ctx.field.map(f64::from);
        let w = (f64::from(ctx.width) / (f[2] - f[0])).round();
        let h = (f64::from(ctx.height) / (f[3] - f[1])).round();
        let fy = h / 2.0 / (self.scene.yfov.unwrap_or(1.0) / 2.0).tan();
        Lens {
            width: w as u32,
            height: h as u32,
            fx: fy,
            fy,
            cx: (w - 1.0) / 2.0,
            cy: (h - 1.0) / 2.0,
            model: Model::Pinhole {
                k: [0.0; 6],
                p: [0.0; 2],
            },
        }
    }

    fn key(ctx: &FrameContext) -> (u32, u32, [u32; 4]) {
        (ctx.width, ctx.height, ctx.field.map(f32::to_bits))
    }

    /// The lens pixel of a mode pixel, and back.
    fn lens_pixel(lens: &Lens, ctx: &FrameContext, x: f64, y: f64) -> (f64, f64) {
        let f = ctx.field.map(f64::from);
        let u = f[0] + (x + 0.5) / f64::from(ctx.width) * (f[2] - f[0]);
        let v = f[1] + (y + 0.5) / f64::from(ctx.height) * (f[3] - f[1]);
        (
            u * f64::from(lens.width) - 0.5,
            v * f64::from(lens.height) - 0.5,
        )
    }

    fn mode_pixel(lens: &Lens, ctx: &FrameContext, [u, v]: [f64; 2]) -> [f64; 2] {
        let f = ctx.field.map(f64::from);
        let fu = (u + 0.5) / f64::from(lens.width);
        let fv = (v + 0.5) / f64::from(lens.height);
        [
            (fu - f[0]) / (f[2] - f[0]) * f64::from(ctx.width) - 0.5,
            (fv - f[1]) / (f[3] - f[1]) * f64::from(ctx.height) - 0.5,
        ]
    }

    fn rays(&self, ctx: &FrameContext, lens: &Lens) -> Arc<Vec<Vec3>> {
        let key = Self::key(ctx);
        let mut cache = self.rays.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, r)) = &*cache
            && *k == key
        {
            return Arc::clone(r);
        }
        let mut rays = Vec::with_capacity((ctx.width * ctx.height) as usize);
        for y in 0..ctx.height {
            for x in 0..ctx.width {
                let (u, v) = Self::lens_pixel(lens, ctx, f64::from(x), f64::from(y));
                rays.push(lens.unproject(u, v));
            }
        }
        let rays = Arc::new(rays);
        *cache = Some((key, Arc::clone(&rays)));
        rays
    }

    /// The camera at `t` s: its position and the optical frame's rotation (x right, y down,
    /// z forward) in the world.
    fn camera_at(&self, t: f64) -> (Vec3, Mat3) {
        let t = if self.scene.duration > 0.0 {
            t.rem_euclid(self.scene.duration)
        } else {
            0.0
        };
        let chain: Vec<_> = self.scene.camera.iter().map(|n| local_at(n, t)).collect();
        let (r, p) = split(&compose_all(&chain));
        // glTF cameras: -Z forward, +Y up.
        let optical = [0, 1, 2].map(|i| [r[i][0], -r[i][1], -r[i][2]]);
        (p, optical)
    }

    fn colour(&self, tri: u32, u: f64, v: f64) -> [f32; 3] {
        let t = &self.scene.tris[tri as usize];
        let m: &Material = &self.scene.materials[t.material as usize];
        let Some(tex) = m.texture.and_then(|i| self.scene.textures.get(i as usize)) else {
            return m.color;
        };
        let w = (1.0 - u - v) as f32;
        let (u, v) = (u as f32, v as f32);
        let uv = [0, 1].map(|k| w * t.uv[0][k] + u * t.uv[1][k] + v * t.uv[2][k]);
        let c = sample(tex, uv);
        [0, 1, 2].map(|k| c[k] * m.color[k])
    }

    /// Renders the image from a camera at `o` (optical frame `rot`), added into `out`.
    fn render(
        &self,
        ctx: &FrameContext,
        rays: &[Vec3],
        o: Vec3,
        rot: &Mat3,
        aa: bool,
        out: &mut [[f32; 3]],
    ) {
        let (w, h) = (ctx.width as usize, ctx.height as usize);
        let offsets: &[(f64, f64)] = if aa {
            &[(-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25)]
        } else {
            &[(0.0, 0.0)]
        };
        let scale = (LIGHT * self.light) as f32 / offsets.len() as f32;
        let rows = h.div_ceil(self.threads.max(1));
        std::thread::scope(|s| {
            for (chunk, part) in out.chunks_mut(rows * w).enumerate() {
                s.spawn(move || {
                    for (i, px) in part.iter_mut().enumerate() {
                        let (x, y) = (i % w, chunk * rows + i / w);
                        let mut sum = [0.0f32; 3];
                        for (dx, dy) in offsets {
                            let d = mul(rot, sub_ray(rays, w, h, x, y, *dx, *dy));
                            let c = match self.bvh.hit(&self.scene.tris, o, d, 1e-6, f64::INFINITY)
                            {
                                Some(hit) => self.colour(hit.tri, hit.u, hit.v),
                                None => [self.background; 3],
                            };
                            for k in 0..3 {
                                sum[k] += c[k];
                            }
                        }
                        for k in 0..3 {
                            px[k] += sum[k] * scale;
                        }
                    }
                });
            }
        });
    }

    fn truth(&self, ctx: &FrameContext, lens: &Lens, o: Vec3, rot: &Mat3) -> GroundTruth {
        let (w, h) = (f64::from(ctx.width), f64::from(ctx.height));
        let points = self
            .scene
            .points
            .iter()
            .filter_map(|(name, p)| {
                let rel = sub(*p, o);
                let [x, y] = Self::mode_pixel(lens, ctx, lens.project(mul_t(rot, rel))?);
                if !(-0.5..w - 0.5).contains(&x) || !(-0.5..h - 0.5).contains(&y) {
                    return None;
                }
                let dist = rel.iter().map(|c| c * c).sum::<f64>().sqrt();
                let d = rel.map(|c| c / dist.max(1e-12));
                let hidden = self
                    .bvh
                    .hit(&self.scene.tris, o, d, 1e-6, dist * (1.0 - 1e-4) - 1e-4)
                    .is_some();
                (!hidden).then(|| (name.clone(), [x, y]))
            })
            .collect();
        // The camera body as glTF holds it (-Z forward, +Y up).
        let body = [0, 1, 2].map(|i| [rot[i][0], -rot[i][1], -rot[i][2]]);
        GroundTruth {
            camera: (o, to_quaternion(&body)),
            points,
        }
    }

    fn frame_now(&self, ctx: &FrameContext) -> (Arc<SceneFrame>, GroundTruth) {
        let lens = self.lens(ctx);
        let rays = self.rays(ctx, &lens);
        let moves = self.moves();
        let samples = if moves && self.blur {
            ((ctx.exposure / 0.002).ceil() as usize).clamp(1, self.blur_samples)
        } else {
            1
        };
        let aa = self.antialias.unwrap_or(!moves);
        let mut light = vec![[0.0f32; 3]; rays.len()];
        for i in 0..samples {
            let t = ctx.time + ctx.exposure * (i as f64 + 0.5) / samples as f64;
            let (o, rot) = self.camera_at(t);
            self.render(ctx, &rays, o, &rot, aa, &mut light);
        }
        if samples > 1 {
            let k = 1.0 / samples as f32;
            light.iter_mut().for_each(|p| *p = p.map(|c| c * k));
        }
        let (o, rot) = self.camera_at(ctx.time + ctx.exposure / 2.0);
        let truth = self.truth(ctx, &lens, o, &rot);
        (
            Arc::new(SceneFrame::Rgb {
                width: ctx.width,
                height: ctx.height,
                light: Arc::new(light),
                spot: None,
            }),
            truth,
        )
    }
}

/// A node's transform at `t` s (its animation tracks over its rest transform).
fn local_at(n: &NodeTransform, t: f64) -> [[f64; 4]; 3] {
    let translation = n.translation_track.as_ref().map_or(n.translation, |tr| {
        sample_track(tr, t, |a, b, f| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * f))
    });
    let rotation = n.rotation_track.as_ref().map_or(n.rotation, |tr| {
        sample_track(tr, t, |a, mut b, f| {
            if (0..4).map(|i| a[i] * b[i]).sum::<f64>() < 0.0 {
                b = b.map(|v| -v);
            }
            [0, 1, 2, 3].map(|i| a[i] + (b[i] - a[i]) * f)
        })
    });
    trs(translation, rotation, n.scale)
}

fn sample_track<T: Copy>(tr: &Track<T>, t: f64, lerp: impl Fn(T, T, f64) -> T) -> T {
    let i = tr.times.iter().rposition(|k| *k <= t).unwrap_or(0);
    let a = tr.values[i.min(tr.values.len() - 1)];
    match (tr.times.get(i + 1), tr.values.get(i + 1)) {
        (Some(next), Some(&b)) if !tr.step => {
            let f = ((t - tr.times[i]) / (next - tr.times[i]).max(1e-12)).clamp(0.0, 1.0);
            lerp(a, b, f)
        }
        _ => a,
    }
}

/// A texture's colour at `uv` (its sampler's filter and wrapping).
fn sample(tex: &Texture, uv: [f32; 2]) -> [f32; 3] {
    let wrap = |c: f32, mode: Wrap, n: u32| -> f32 {
        let n = n as f32;
        match mode {
            Wrap::Repeat => c.rem_euclid(1.0) * n,
            Wrap::Clamp => c.clamp(0.0, 1.0) * n,
            Wrap::Mirror => {
                let m = c.rem_euclid(2.0);
                (if m > 1.0 { 2.0 - m } else { m }) * n
            }
        }
    };
    let (x, y) = (
        wrap(uv[0], tex.wrap[0], tex.width),
        wrap(uv[1], tex.wrap[1], tex.height),
    );
    let at = |x: i64, y: i64| {
        let x = x.clamp(0, i64::from(tex.width) - 1) as usize;
        let y = y.clamp(0, i64::from(tex.height) - 1) as usize;
        tex.texels[y * tex.width as usize + x]
    };
    if tex.nearest {
        return at(x.floor() as i64, y.floor() as i64);
    }
    let (fx, fy) = (x - 0.5, y - 0.5);
    let (x0, y0) = (fx.floor(), fy.floor());
    let (ax, ay) = (fx - x0, fy - y0);
    let (x0, y0) = (x0 as i64, y0 as i64);
    let (a, b, c, d) = (
        at(x0, y0),
        at(x0 + 1, y0),
        at(x0, y0 + 1),
        at(x0 + 1, y0 + 1),
    );
    [0, 1, 2].map(|k| {
        let top = a[k] + (b[k] - a[k]) * ax;
        let bottom = c[k] + (d[k] - c[k]) * ax;
        top + (bottom - top) * ay
    })
}

/// The ray at `(x + dx, y + dy)`, between the pixels' rays.
fn sub_ray(rays: &[Vec3], w: usize, h: usize, x: usize, y: usize, dx: f64, dy: f64) -> Vec3 {
    let at = |x: usize, y: usize| rays[y.min(h - 1) * w + x.min(w - 1)];
    let r = at(x, y);
    if dx == 0.0 && dy == 0.0 {
        return r;
    }
    let nx = if dx >= 0.0 {
        at(x + 1, y)
    } else {
        at(x.saturating_sub(1), y)
    };
    let ny = if dy >= 0.0 {
        at(x, y + 1)
    } else {
        at(x, y.saturating_sub(1))
    };
    let (ax, ay) = (dx.abs(), dy.abs());
    [0, 1, 2].map(|i| r[i] + (nx[i] - r[i]) * ax + (ny[i] - r[i]) * ay)
}

impl SimScene for GltfScene {
    fn frame(&self, index: u64, width: u32, height: u32, field: [f32; 4]) -> Arc<SceneFrame> {
        let ctx = FrameContext {
            index,
            time: index as f64 / 30.0,
            exposure: 0.01,
            width,
            height,
            field,
        };
        self.frame_at(&ctx).0
    }

    fn frame_at(&self, ctx: &FrameContext) -> (Arc<SceneFrame>, Option<GroundTruth>) {
        if self.moves() {
            let (f, t) = self.frame_now(ctx);
            return (f, Some(t));
        }
        let key = Self::key(ctx);
        let mut still = self.still.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, f, t)) = &*still
            && *k == key
        {
            return (Arc::clone(f), Some(t.clone()));
        }
        let (f, t) = self.frame_now(ctx);
        *still = Some((key, Arc::clone(&f), t.clone()));
        (f, Some(t))
    }
}

#[cfg(test)]
mod tests;
