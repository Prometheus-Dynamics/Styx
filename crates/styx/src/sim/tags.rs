//! `scene=tags`: AprilTag 36h11 tags at their poses in 3D, seen through the camera's lens
//! (its calibration: the fisheye or the pinhole model) from the camera's pose or trajectory,
//! with motion blur over the exposure when the camera moves, and each frame's ground truth
//! (the camera's pose, every visible tag's corners).
//!
//! Each pixel's ray comes from the lens model (computed once per mode); inside each tag's
//! projected bounds the ray meets the tag's plane and reads the tag's cell (2x2 samples per
//! pixel), elsewhere a floor (z = 0, a 1 m checker) or a plain wall.

use std::path::Path;
use std::sync::{Arc, Mutex};

use styx_native::sim::{FrameContext, GroundTruth, SceneFrame, SimScene};

use super::files::{CameraPath, LayoutTag, calibration, camera, inline_pose, layout};
use super::geometry::{
    Lens, Mat3, Model, OPTICAL, Pose, Vec3, dot, matmul, mul, mul_t, sub, to_quaternion,
};
use super::tag36h11::CODES;

/// FRC tags: 6.5 in across the black square.
const FRC_TAG_SIZE: f64 = 0.1651;
/// Light per second at gain 1 on a white surface (as the chart: 18% grey at 18% of full scale
/// at 10 ms).
const LIGHT: f64 = 100.0;
const WHITE: f64 = 0.8;
const BLACK: f64 = 0.04;

/// The rays of a mode's pixels, and what they were made for (size, field bits).
type Rays = ((u32, u32, [u32; 4]), Arc<Vec<Vec3>>);
/// A still camera's frame, and what it was made for.
type Still = ((u32, u32, [u32; 4]), Arc<SceneFrame>, GroundTruth);

pub(crate) struct TagsScene {
    tags: Vec<LayoutTag>,
    lens: Lens,
    camera: CameraPath,
    light: f64,
    antialias: bool,
    blur: bool,
    rays: Mutex<Option<Rays>>,
    still: Mutex<Option<Still>>,
}

/// A plausible OV9782 module with an M12 fisheye at 1280x800, for scenes given no calibration.
fn default_lens() -> Lens {
    Lens {
        width: 1280,
        height: 800,
        fx: 560.0,
        fy: 560.0,
        cx: 639.5,
        cy: 399.5,
        model: Model::Fisheye([0.0; 4]),
    }
}

/// Four tags (ids 1 to 4) on a wall 3 m ahead of a camera at the origin, 0.5 m up.
fn default_layout(size: f64) -> Vec<LayoutTag> {
    let facing = super::geometry::from_rpy(0.0, 0.0, std::f64::consts::PI);
    [(1, -0.5, 0.8), (2, 0.5, 0.8), (3, -0.5, 0.3), (4, 0.5, 0.3)]
        .into_iter()
        .map(|(id, y, z)| LayoutTag {
            id,
            position: [3.0, y, z],
            rotation: facing,
            size,
        })
        .collect()
}

impl TagsScene {
    /// From a spec's parameters: `layout=<WPILib .json | Limelight .fmap>`, `tag_size=<m>`,
    /// `calibration=<HeliOS calibration .json>`, `camera=<pose or trajectory .json>` or
    /// `pose=x:y:z:roll:pitch:yaw`, `light=<f>`, `antialias=on|off`, `blur=on|off`.
    pub(crate) fn from_params(params: &[(String, String)]) -> Result<Self, String> {
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let on = |k: &str| !matches!(get(k), Some("off" | "0" | "false"));
        let size = match get("tag_size") {
            Some(v) => v
                .parse::<f64>()
                .map_err(|_| format!("tag_size={v}: metres"))?,
            None => FRC_TAG_SIZE,
        };
        let tags = match get("layout") {
            Some(p) => layout(Path::new(p), size)?,
            None => default_layout(size),
        };
        let lens = match get("calibration") {
            Some(p) => calibration(Path::new(p))?,
            None => default_lens(),
        };
        let camera = match (get("camera"), get("pose")) {
            (Some(p), _) => camera(Path::new(p))?,
            (None, Some(p)) => CameraPath::Still(inline_pose(p)?),
            (None, None) => CameraPath::Still(Pose::rpy_degrees([0.0, 0.0, 0.5], [0.0; 3])),
        };
        for (k, _) in params {
            if !matches!(
                k.as_str(),
                "layout"
                    | "tag_size"
                    | "calibration"
                    | "camera"
                    | "pose"
                    | "light"
                    | "antialias"
                    | "blur"
            ) {
                return Err(format!("unknown key {k}"));
            }
        }
        Ok(Self {
            tags,
            lens,
            camera,
            light: get("light").and_then(|v| v.parse().ok()).unwrap_or(1.0),
            antialias: on("antialias"),
            blur: on("blur"),
            rays: Mutex::new(None),
            still: Mutex::new(None),
        })
    }

    /// The calibration's pixel of a mode pixel, and back.
    fn lens_pixel(&self, ctx: &FrameContext, x: f64, y: f64) -> (f64, f64) {
        let f = ctx.field.map(f64::from);
        let u = f[0] + (x + 0.5) / f64::from(ctx.width) * (f[2] - f[0]);
        let v = f[1] + (y + 0.5) / f64::from(ctx.height) * (f[3] - f[1]);
        (
            u * f64::from(self.lens.width) - 0.5,
            v * f64::from(self.lens.height) - 0.5,
        )
    }

    fn mode_pixel(&self, ctx: &FrameContext, [u, v]: [f64; 2]) -> [f64; 2] {
        let f = ctx.field.map(f64::from);
        let fu = (u + 0.5) / f64::from(self.lens.width);
        let fv = (v + 0.5) / f64::from(self.lens.height);
        [
            (fu - f[0]) / (f[2] - f[0]) * f64::from(ctx.width) - 0.5,
            (fv - f[1]) / (f[3] - f[1]) * f64::from(ctx.height) - 0.5,
        ]
    }

    fn key(ctx: &FrameContext) -> (u32, u32, [u32; 4]) {
        (ctx.width, ctx.height, ctx.field.map(f32::to_bits))
    }

    /// Each mode pixel's ray (optical frame).
    fn rays(&self, ctx: &FrameContext) -> Arc<Vec<Vec3>> {
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
                let (u, v) = self.lens_pixel(ctx, f64::from(x), f64::from(y));
                rays.push(self.lens.unproject(u, v));
            }
        }
        let rays = Arc::new(rays);
        *cache = Some((key, Arc::clone(&rays)));
        rays
    }

    /// The mode pixel a world point images to, from a camera at `pose`.
    fn image_of(&self, ctx: &FrameContext, pose: &Pose, rot: &Mat3, p: Vec3) -> Option<[f64; 2]> {
        let optical = mul_t(rot, sub(p, pose.position));
        self.lens
            .project(optical)
            .map(|uv| self.mode_pixel(ctx, uv))
    }

    /// The background from a camera at `pose` (optical to world: `rot`): a floor with a 1 m
    /// checker below the horizon, a wall above.
    fn background(&self, rays: &[Vec3], pose: &Pose, rot: &Mat3) -> Vec<f64> {
        let o = pose.position;
        rays.iter()
            .map(|r| {
                let d = mul(rot, *r);
                if d[2] < -1e-6 && o[2] > 0.0 {
                    let t = -o[2] / d[2];
                    let (fx, fy) = (o[0] + t * d[0], o[1] + t * d[1]);
                    if (fx.floor() + fy.floor()).rem_euclid(2.0) < 1.0 {
                        0.12
                    } else {
                        0.18
                    }
                } else {
                    0.35
                }
            })
            .collect()
    }

    /// Draws the tags into `out` (a background) as a camera at `pose` sees them.
    fn draw_tags(
        &self,
        ctx: &FrameContext,
        rays: &[Vec3],
        pose: &Pose,
        rot: &Mat3,
        out: &mut [f64],
    ) {
        let (w, h) = (ctx.width as usize, ctx.height as usize);
        let o = pose.position;
        let offsets: &[(f64, f64)] = if self.antialias {
            &[(-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25)]
        } else {
            &[(0.0, 0.0)]
        };
        for tag in &self.tags {
            let Some(code) = CODES.get(tag.id as usize).copied() else {
                continue;
            };
            let n = mul(&tag.rotation, [1.0, 0.0, 0.0]);
            if dot(sub(o, tag.position), n) <= 0.0 {
                continue; // Seen from behind.
            }
            // Its bounds in the image: the white border's corners (10 cells across).
            let half = tag.size * 0.625;
            let mut bounds = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
            let mut seen = 0;
            for (ly, lz) in [(-half, half), (half, half), (half, -half), (-half, -half)] {
                let p = add(tag.position, mul(&tag.rotation, [0.0, ly, lz]));
                if let Some([x, y]) = self.image_of(ctx, pose, rot, p) {
                    seen += 1;
                    bounds = [
                        bounds[0].min(x),
                        bounds[1].min(y),
                        bounds[2].max(x),
                        bounds[3].max(y),
                    ];
                }
            }
            if seen == 0 {
                continue;
            }
            if seen < 4 {
                bounds = [0.0, 0.0, w as f64, h as f64];
            }
            let x0 = (bounds[0].floor() as i64 - 1).clamp(0, w as i64) as usize;
            let y0 = (bounds[1].floor() as i64 - 1).clamp(0, h as i64) as usize;
            let x1 = (bounds[2].ceil() as i64 + 2).clamp(0, w as i64) as usize;
            let y1 = (bounds[3].ceil() as i64 + 2).clamp(0, h as i64) as usize;
            let cell = tag.size / 8.0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = y * w + x;
                    let (mut sum, mut hit) = (0.0, false);
                    for (dx, dy) in offsets {
                        let ray = sub_ray(rays, w, h, x, y, *dx, *dy);
                        let d = mul(rot, ray);
                        let denom = dot(d, n);
                        let v = if denom < -1e-9 {
                            let t = dot(sub(tag.position, o), n) / denom;
                            let p = add(o, d.map(|c| c * t));
                            let local = mul_t(&tag.rotation, sub(p, tag.position));
                            cell_at(code, local[1], local[2], cell)
                        } else {
                            None
                        };
                        hit |= v.is_some();
                        sum += v.unwrap_or(out[i]);
                    }
                    if hit {
                        out[i] = sum / offsets.len() as f64;
                    }
                }
            }
        }
    }

    /// The ground truth seen from a camera at `pose`.
    fn truth(&self, ctx: &FrameContext, pose: &Pose, rot: &Mat3) -> GroundTruth {
        let (w, h) = (f64::from(ctx.width), f64::from(ctx.height));
        let mut tags = Vec::new();
        for tag in &self.tags {
            let n = mul(&tag.rotation, [1.0, 0.0, 0.0]);
            if dot(sub(pose.position, tag.position), n) <= 0.0 {
                continue;
            }
            let s = tag.size / 2.0;
            // Top left, top right, bottom right, bottom left of the tag as printed.
            let corners: Option<Vec<[f64; 2]>> = [(-s, s), (s, s), (s, -s), (-s, -s)]
                .into_iter()
                .map(|(ly, lz)| {
                    let p = add(tag.position, mul(&tag.rotation, [0.0, ly, lz]));
                    self.image_of(ctx, pose, rot, p)
                        .filter(|[x, y]| (-0.5..w - 0.5).contains(x) && (-0.5..h - 0.5).contains(y))
                })
                .collect();
            if let Some(c) = corners {
                tags.push((tag.id, [c[0], c[1], c[2], c[3]]));
            }
        }
        GroundTruth {
            camera: (pose.position, to_quaternion(&pose.rotation)),
            tags,
        }
    }

    fn frame_now(&self, ctx: &FrameContext) -> (Arc<SceneFrame>, GroundTruth) {
        let rays = self.rays(ctx);
        let moves = self.camera.moves();
        // Over the exposure: one pose per 2 ms, up to 8, for a moving camera.
        let samples = if moves && self.blur {
            ((ctx.exposure / 0.002).ceil() as usize).clamp(1, 8)
        } else {
            1
        };
        // The background once (from the middle of the exposure: it has no detail to blur), the
        // tags at each pose over the exposure.
        let mid = self.camera.at(ctx.time + ctx.exposure / 2.0);
        let background = self.background(&rays, &mid, &matmul(&mid.rotation, &OPTICAL));
        let mut light = vec![0.0f64; rays.len()];
        let mut sample = background.clone();
        for i in 0..samples {
            let t = ctx.time + ctx.exposure * (i as f64 + 0.5) / samples as f64;
            let pose = self.camera.at(t);
            let rot = matmul(&pose.rotation, &OPTICAL);
            sample.copy_from_slice(&background);
            self.draw_tags(ctx, &rays, &pose, &rot, &mut sample);
            for (a, b) in light.iter_mut().zip(&sample) {
                *a += b;
            }
        }
        let scale = LIGHT * self.light / samples as f64;
        let light: Vec<[f32; 3]> = light.iter().map(|v| [(v * scale) as f32; 3]).collect();
        let truth = self.truth(ctx, &mid, &matmul(&mid.rotation, &OPTICAL));
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

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
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

/// The tag's reflectance at `(ly, lz)` on its face (right, up; cells of `cell` m), or `None`
/// off its white border. The black square is 8 cells across (a 6x6 code inside a 1-cell
/// black border), the white border 1 cell around it.
fn cell_at(code: u64, ly: f64, lz: f64, cell: f64) -> Option<f64> {
    let col = ((ly / cell) + 5.0).floor();
    let row = (5.0 - (lz / cell)).floor();
    if !(0.0..10.0).contains(&col) || !(0.0..10.0).contains(&row) {
        return None;
    }
    let (c, r) = (col as u32, row as u32);
    if c == 0 || c == 9 || r == 0 || r == 9 {
        return Some(WHITE);
    }
    if c == 1 || c == 8 || r == 1 || r == 8 {
        return Some(BLACK);
    }
    let bit = (r - 2) * 6 + (c - 2);
    Some(if code >> (35 - bit) & 1 == 1 {
        WHITE
    } else {
        BLACK
    })
}

impl SimScene for TagsScene {
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
        if self.camera.moves() {
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
mod tests {
    use super::*;

    fn ctx() -> FrameContext {
        FrameContext {
            index: 0,
            time: 0.0,
            exposure: 0.01,
            width: 640,
            height: 400,
            field: [0.0, 0.0, 1.0, 1.0],
        }
    }

    /// Tag 0's top left data cell, and its borders.
    #[test]
    fn cells_follow_the_code() {
        let cell = 1.0;
        // The outer white border and the black border.
        assert_eq!(cell_at(CODES[0], -4.5, 4.5, cell), Some(WHITE));
        assert_eq!(cell_at(CODES[0], -3.5, 3.5, cell), Some(BLACK));
        // Data cell (0, 0): the code's first bit.
        let first = CODES[0] >> 35 & 1;
        assert_eq!(
            cell_at(CODES[0], -2.5, 2.5, cell),
            Some(if first == 1 { WHITE } else { BLACK })
        );
        assert_eq!(cell_at(CODES[0], 5.5, 0.0, cell), None);
    }

    /// The default scene: four tags in view, their corners where the lens puts them, the
    /// image dark on the black borders and bright on the white ones.
    #[test]
    fn the_default_tags_are_seen_through_the_lens() {
        let scene = TagsScene::from_params(&[]).unwrap();
        let (frame, truth) = scene.frame_at(&ctx());
        let truth = truth.unwrap();
        let mut ids: Vec<u32> = truth.tags.iter().map(|t| t.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2, 3, 4]);
        let SceneFrame::Rgb { light, .. } = &*frame else {
            panic!("rgb");
        };
        // Tag 1 (upper right, as seen: the tags face the camera): just inside its black border and
        // just outside it.
        let (_, c) = truth.tags.iter().find(|t| t.0 == 1).unwrap();
        let [tl, tr, br, _] = *c;
        assert!(tl[0] < tr[0] && tl[1] < br[1], "{c:?}");
        let at = |x: f64, y: f64| light[(y.round() as usize) * 640 + x.round() as usize][0];
        let inside = |f: f64| [tl[0] + (br[0] - tl[0]) * f, tl[1] + (br[1] - tl[1]) * f];
        let [bx, by] = inside(0.06);
        assert!(
            at(bx, by) < (BLACK * LIGHT + 1.0) as f32,
            "black border {}",
            at(bx, by)
        );
        let [wx, wy] = [
            tl[0] - (br[0] - tl[0]) * 0.06,
            tl[1] - (br[1] - tl[1]) * 0.06,
        ];
        assert!(
            at(wx, wy) > (WHITE * LIGHT * 0.9) as f32,
            "white border {}",
            at(wx, wy)
        );
        // The camera at its pose.
        assert_eq!(truth.camera.0, [0.0, 0.0, 0.5]);
    }
}

#[cfg(test)]
mod dump {
    use super::*;

    /// Writes rendered frames and their truth to `STYX_SIM_DUMP` (a directory), for checking
    /// with another detector (`cargo test -p styx --features sim dump_tags -- --ignored`).
    #[test]
    #[ignore]
    fn dump_tags() {
        let Some(dir) = std::env::var_os("STYX_SIM_DUMP") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let poses = [
            "0:0:0.5:0:0:0",
            "0.5:1.0:0.6:0:0:-25",
            "1.2:-0.8:0.9:5:12:30",
            "2.2:0.1:0.5:0:0:0",
        ];
        for (i, pose) in poses.iter().enumerate() {
            let params = vec![("pose".to_string(), pose.to_string())];
            let scene = TagsScene::from_params(&params).unwrap();
            let ctx = FrameContext {
                width: 1280,
                height: 800,
                ..super::tests_ctx()
            };
            let (frame, truth) = scene.frame_at(&ctx);
            let SceneFrame::Rgb { light, .. } = &*frame else {
                panic!("rgb");
            };
            let mut pgm = b"P5\n1280 800\n255\n".to_vec();
            pgm.extend(
                light
                    .iter()
                    .map(|l| ((l[0] / 100.0).clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8),
            );
            std::fs::write(dir.join(format!("tags{i}.pgm")), pgm).unwrap();
            let t = truth.unwrap();
            let lines: Vec<String> = t
                .tags
                .iter()
                .map(|(id, c)| {
                    format!(
                        "{id} {}",
                        c.iter()
                            .map(|p| format!("{:.3} {:.3}", p[0], p[1]))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                })
                .collect();
            std::fs::write(dir.join(format!("tags{i}.txt")), lines.join("\n")).unwrap();
        }
    }
}

#[cfg(test)]
fn tests_ctx() -> FrameContext {
    FrameContext {
        index: 0,
        time: 0.0,
        exposure: 0.01,
        width: 640,
        height: 400,
        field: [0.0, 0.0, 1.0, 1.0],
    }
}

#[cfg(test)]
mod speed {
    use super::*;

    /// Frame time of the default tags with a moving camera (blur), 1280x800 (`cargo test
    /// --release -p styx --features sim tags_speed -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn tags_speed() {
        let dir = std::env::temp_dir().join(format!("styx-tags-speed-{}.json", std::process::id()));
        std::fs::write(
            &dir,
            r#"{"trajectory":[{"t":0,"x":0,"z":0.5},{"t":2,"x":1.5,"z":0.5,"yaw":20},{"t":4,"x":0,"z":0.5}]}"#,
        )
        .unwrap();
        let params = vec![("camera".to_string(), dir.display().to_string())];
        let scene = TagsScene::from_params(&params).unwrap();
        let ctx = |i: u64| FrameContext {
            index: i,
            time: i as f64 / 30.0,
            exposure: 0.01,
            width: 1280,
            height: 800,
            field: [0.0, 0.0, 1.0, 1.0],
        };
        scene.frame_at(&ctx(0));
        let t = std::time::Instant::now();
        for i in 1..21 {
            std::hint::black_box(scene.frame_at(&ctx(i)));
        }
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "moving camera, 5 blur samples: {:.1} ms per frame",
            t.elapsed().as_secs_f64() * 1e3 / 20.0
        );
        let _ = std::fs::remove_file(dir);
    }
}
