//! Reading a glTF scene: its triangles in world space with their texture coordinates and
//! materials (base colour factor and texture), its camera (a node with a camera, or one named)
//! with the transforms and animation tracks from the scene's root down to it, and its named
//! points (`truth:` nodes).

use std::path::Path;

use super::super::geometry::{Mat3, Vec3, from_quaternion};

/// A triangle: a vertex and the two edges from it, the vertices' texture coordinates, its
/// material.
#[derive(Clone, Copy, Debug)]
pub(super) struct Tri {
    pub(super) v0: Vec3,
    pub(super) e1: Vec3,
    pub(super) e2: Vec3,
    pub(super) uv: [[f32; 2]; 3],
    pub(super) material: u32,
}

/// A material: its base colour (linear) and texture.
#[derive(Clone, Copy, Debug)]
pub(super) struct Material {
    pub(super) color: [f32; 3],
    pub(super) texture: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Wrap {
    Repeat,
    Clamp,
    Mirror,
}

/// A texture: linear colour per texel, the sampler's filter and wrapping.
#[derive(Clone, Debug)]
pub(super) struct Texture {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) texels: Vec<[f32; 3]>,
    pub(super) nearest: bool,
    pub(super) wrap: [Wrap; 2],
}

/// Keyframes of one animated property (linear, or held until the next key).
#[derive(Clone, Debug)]
pub(super) struct Track<T> {
    pub(super) times: Vec<f64>,
    pub(super) values: Vec<T>,
    pub(super) step: bool,
}

/// A node's transform at rest and its animation tracks.
#[derive(Clone, Debug)]
pub(super) struct NodeTransform {
    pub(super) translation: Vec3,
    /// w, x, y, z.
    pub(super) rotation: [f64; 4],
    pub(super) scale: Vec3,
    pub(super) translation_track: Option<Track<Vec3>>,
    pub(super) rotation_track: Option<Track<[f64; 4]>>,
}

/// What a scene file holds for rendering.
pub(super) struct Loaded {
    pub(super) tris: Vec<Tri>,
    pub(super) materials: Vec<Material>,
    pub(super) textures: Vec<Texture>,
    /// The camera's node and its ancestors, root first.
    pub(super) camera: Vec<NodeTransform>,
    /// The camera's vertical field of view (radians), when it is a perspective camera.
    pub(super) yfov: Option<f64>,
    /// The longest animation (s).
    pub(super) duration: f64,
    /// Named points (`truth:<name>` nodes) at rest, world positions.
    pub(super) points: Vec<(String, Vec3)>,
}

/// A 4x4 transform, rows of the upper 3x4 (the last row is 0, 0, 0, 1).
type Affine = [[f64; 4]; 3];

const IDENTITY: Affine = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

fn compose(a: &Affine, b: &Affine) -> Affine {
    [0, 1, 2].map(|r| {
        [0, 1, 2, 3].map(|c| {
            let mut v: f64 = (0..3).map(|k| a[r][k] * b[k][c]).sum();
            if c == 3 {
                v += a[r][3];
            }
            v
        })
    })
}

fn apply(a: &Affine, p: Vec3) -> Vec3 {
    [0, 1, 2].map(|r| a[r][0] * p[0] + a[r][1] * p[1] + a[r][2] * p[2] + a[r][3])
}

/// The transform of translation, rotation (w, x, y, z) and scale.
pub(super) fn trs(t: Vec3, r: [f64; 4], s: Vec3) -> Affine {
    let m = from_quaternion(r);
    [0, 1, 2].map(|i| [m[i][0] * s[0], m[i][1] * s[1], m[i][2] * s[2], t[i]])
}

/// The rotation of a transform (its scale taken out) and its translation.
pub(super) fn split(a: &Affine) -> (Mat3, Vec3) {
    let mut m: Mat3 = [0, 1, 2].map(|r| [a[r][0], a[r][1], a[r][2]]);
    for c in 0..3 {
        let n = (0..3)
            .map(|r| m[r][c] * m[r][c])
            .sum::<f64>()
            .sqrt()
            .max(1e-12);
        for row in &mut m {
            row[c] /= n;
        }
    }
    (m, [a[0][3], a[1][3], a[2][3]])
}

pub(super) fn compose_all(chain: &[Affine]) -> Affine {
    chain.iter().fold(IDENTITY, |acc, a| compose(&acc, a))
}

fn local(node: &gltf::Node<'_>) -> Affine {
    let (t, r, s) = node.transform().decomposed();
    trs(
        t.map(f64::from),
        [r[3], r[0], r[1], r[2]].map(f64::from),
        s.map(f64::from),
    )
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn texels(image: &gltf::image::Data) -> Result<Vec<[f32; 3]>, String> {
    use gltf::image::Format;
    let p = &image.pixels;
    let (step, wide) = match image.format {
        Format::R8 => (1, false),
        Format::R8G8 => (2, false),
        Format::R8G8B8 => (3, false),
        Format::R8G8B8A8 => (4, false),
        Format::R16 => (1, true),
        Format::R16G16 => (2, true),
        Format::R16G16B16 => (3, true),
        Format::R16G16B16A16 => (4, true),
        other => return Err(format!("texture format {other:?}")),
    };
    let bytes = if wide { 2 } else { 1 };
    let value = |i: usize| -> f32 {
        if wide {
            f32::from(u16::from_le_bytes([p[i * 2], p[i * 2 + 1]])) / 65535.0
        } else {
            f32::from(p[i]) / 255.0
        }
    };
    let n = p.len() / (step * bytes);
    Ok((0..n)
        .map(|i| {
            let c =
                |k: usize| srgb_to_linear(value(i * step + k.min(if step >= 3 { 2 } else { 0 })));
            [c(0), c(1), c(2)]
        })
        .collect())
}

fn wrap(mode: gltf::texture::WrappingMode) -> Wrap {
    match mode {
        gltf::texture::WrappingMode::ClampToEdge => Wrap::Clamp,
        gltf::texture::WrappingMode::MirroredRepeat => Wrap::Mirror,
        gltf::texture::WrappingMode::Repeat => Wrap::Repeat,
    }
}

/// Reads `path` (`.gltf` or `.glb`); the camera is the node named `camera_name`, else the
/// first node with a camera.
pub(super) fn load(path: &Path, camera_name: Option<&str>) -> Result<Loaded, String> {
    let (doc, buffers, images) =
        gltf::import(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let textures = doc
        .textures()
        .map(|t| {
            let image = images
                .get(t.source().index())
                .ok_or_else(|| format!("texture {} has no image", t.index()))?;
            let s = t.sampler();
            Ok(Texture {
                width: image.width,
                height: image.height,
                texels: texels(image)?,
                nearest: s.mag_filter() == Some(gltf::texture::MagFilter::Nearest),
                wrap: [wrap(s.wrap_s()), wrap(s.wrap_t())],
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut materials: Vec<Material> = doc
        .materials()
        .map(|m| {
            let pbr = m.pbr_metallic_roughness();
            let f = pbr.base_color_factor();
            Material {
                color: [f[0], f[1], f[2]],
                texture: pbr.base_color_texture().map(|t| t.texture().index() as u32),
            }
        })
        .collect();
    // glTF's default material: white.
    let default_material = materials.len() as u32;
    materials.push(Material {
        color: [1.0; 3],
        texture: None,
    });

    // Animation tracks by node.
    let mut duration = 0.0f64;
    let mut translations: Vec<(usize, Track<Vec3>)> = Vec::new();
    let mut rotations: Vec<(usize, Track<[f64; 4]>)> = Vec::new();
    for anim in doc.animations() {
        for ch in anim.channels() {
            let node = ch.target().node().index();
            let step = ch.sampler().interpolation() == gltf::animation::Interpolation::Step;
            let reader = ch.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let Some(times) = reader.read_inputs() else {
                continue;
            };
            let times: Vec<f64> = times.map(f64::from).collect();
            duration = duration.max(times.last().copied().unwrap_or(0.0));
            let cubic = ch.sampler().interpolation() == gltf::animation::Interpolation::CubicSpline;
            match reader.read_outputs() {
                Some(gltf::animation::util::ReadOutputs::Translations(v)) => {
                    let mut values: Vec<Vec3> = v.map(|t| t.map(f64::from)).collect();
                    if cubic {
                        // In-tangent, value, out-tangent per key: the values.
                        values = values.chunks(3).filter_map(|c| c.get(1).copied()).collect();
                    }
                    translations.push((
                        node,
                        Track {
                            times: times.clone(),
                            values,
                            step,
                        },
                    ));
                }
                Some(gltf::animation::util::ReadOutputs::Rotations(r)) => {
                    let mut values: Vec<[f64; 4]> = r
                        .into_f32()
                        .map(|q| [q[3], q[0], q[1], q[2]].map(f64::from))
                        .collect();
                    if cubic {
                        values = values.chunks(3).filter_map(|c| c.get(1).copied()).collect();
                    }
                    rotations.push((
                        node,
                        Track {
                            times,
                            values,
                            step,
                        },
                    ));
                }
                _ => {}
            }
        }
    }

    let scene = doc
        .default_scene()
        .or_else(|| doc.scenes().next())
        .ok_or("no scene")?;
    let mut tris = Vec::new();
    let mut points = Vec::new();
    let mut camera_chain: Option<Vec<usize>> = None;
    let mut yfov = None;
    // Depth first: (node, its transform in the world, the path of node indices to it).
    let mut stack: Vec<(gltf::Node<'_>, Affine, Vec<usize>)> =
        scene.nodes().map(|n| (n, IDENTITY, Vec::new())).collect();
    while let Some((node, parent, mut path)) = stack.pop() {
        let world = compose(&parent, &local(&node));
        path.push(node.index());
        let name = node.name().unwrap_or("");
        if let Some(point) = name.strip_prefix("truth:") {
            points.push((point.to_owned(), [world[0][3], world[1][3], world[2][3]]));
        }
        let wanted = match camera_name {
            Some(n) => name == n,
            None => node.camera().is_some(),
        };
        if wanted && camera_chain.is_none() {
            camera_chain = Some(path.clone());
            if let Some(cam) = node.camera()
                && let gltf::camera::Projection::Perspective(p) = cam.projection()
            {
                yfov = Some(f64::from(p.yfov()));
            }
        }
        if let Some(mesh) = node.mesh() {
            for prim in mesh.primitives() {
                if prim.mode() != gltf::mesh::Mode::Triangles {
                    continue;
                }
                let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
                let Some(positions) = reader.read_positions() else {
                    continue;
                };
                let positions: Vec<Vec3> =
                    positions.map(|p| apply(&world, p.map(f64::from))).collect();
                let uvs: Vec<[f32; 2]> = reader
                    .read_tex_coords(0)
                    .map(|t| t.into_f32().collect())
                    .unwrap_or_else(|| vec![[0.0; 2]; positions.len()]);
                let indices: Vec<u32> = reader
                    .read_indices()
                    .map(|i| i.into_u32().collect())
                    .unwrap_or_else(|| (0..positions.len() as u32).collect());
                let material = prim
                    .material()
                    .index()
                    .map_or(default_material, |i| i as u32);
                for t in indices.as_chunks::<3>().0 {
                    let [a, b, c] = t.map(|i| i as usize);
                    let (Some(&pa), Some(&pb), Some(&pc)) =
                        (positions.get(a), positions.get(b), positions.get(c))
                    else {
                        continue;
                    };
                    let uv = |i: usize| uvs.get(i).copied().unwrap_or([0.0; 2]);
                    tris.push(Tri {
                        v0: pa,
                        e1: super::super::geometry::sub(pb, pa),
                        e2: super::super::geometry::sub(pc, pa),
                        uv: [uv(a), uv(b), uv(c)],
                        material,
                    });
                }
            }
        }
        for child in node.children() {
            stack.push((child, world, path.clone()));
        }
    }
    let chain = camera_chain.ok_or_else(|| match camera_name {
        Some(n) => format!("no node named {n}"),
        None => "no camera in the scene (a node with a camera, or camera=<node name>)".to_owned(),
    })?;
    let nodes: Vec<gltf::Node<'_>> = doc.nodes().collect();
    let camera = chain
        .iter()
        .map(|&i| {
            let (t, r, s) = nodes[i].transform().decomposed();
            NodeTransform {
                translation: t.map(f64::from),
                rotation: [r[3], r[0], r[1], r[2]].map(f64::from),
                scale: s.map(f64::from),
                translation_track: translations
                    .iter()
                    .find(|(n, _)| *n == i)
                    .map(|(_, t)| t.clone()),
                rotation_track: rotations
                    .iter()
                    .find(|(n, _)| *n == i)
                    .map(|(_, t)| t.clone()),
            }
        })
        .collect();
    Ok(Loaded {
        tris,
        materials,
        textures,
        camera,
        yfov,
        duration,
        points,
    })
}
