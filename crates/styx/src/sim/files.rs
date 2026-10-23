//! The files a tags scene reads: where the tags are (a WPILib AprilTag field layout, or a
//! Limelight `.fmap`), the camera's calibration (HeliOS's JSON) and its pose or trajectory.

use std::path::Path;

use serde_json::Value;

use super::geometry::{Lens, Mat3, Model, Pose, from_quaternion};

/// One tag of a layout: its id, the pose of its face (the tag faces its +X axis; seen from
/// the front, its +Y is to the right and +Z up) and its size (the black square's side, m).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LayoutTag {
    pub(crate) id: u32,
    pub(crate) position: [f64; 3],
    pub(crate) rotation: Mat3,
    pub(crate) size: f64,
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// The tags of a layout file: WPILib's JSON (`tags` with `ID` and `pose`; every tag
/// `tag_size` m) or a Limelight `.fmap` (`fiducials` with `id`, `size` in mm and a row-major
/// 4x4 `transform`).
pub(crate) fn layout(path: &Path, tag_size: f64) -> Result<Vec<LayoutTag>, String> {
    let json = read_json(path)?;
    if let Some(tags) = json.get("tags").and_then(Value::as_array) {
        return tags
            .iter()
            .map(|t| {
                let id = t
                    .get("ID")
                    .and_then(Value::as_u64)
                    .ok_or("a tag without an ID")?;
                let pose = t.get("pose").ok_or("a tag without a pose")?;
                let tr = pose
                    .get("translation")
                    .ok_or("a pose without a translation")?;
                let q = pose
                    .get("rotation")
                    .and_then(|r| r.get("quaternion"))
                    .ok_or("a pose without a quaternion")?;
                let get = |v: &Value, k: &str| num(v, k).ok_or(format!("tag {id}: no {k}"));
                Ok(LayoutTag {
                    id: id as u32,
                    position: [get(tr, "x")?, get(tr, "y")?, get(tr, "z")?],
                    rotation: from_quaternion([
                        get(q, "W")?,
                        get(q, "X")?,
                        get(q, "Y")?,
                        get(q, "Z")?,
                    ]),
                    size: tag_size,
                })
            })
            .collect();
    }
    if let Some(fiducials) = json.get("fiducials").and_then(Value::as_array) {
        return fiducials
            .iter()
            .map(|f| {
                let id = f
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or("a fiducial without an id")?;
                let m: Vec<f64> = f
                    .get("transform")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_f64).collect())
                    .filter(|m: &Vec<f64>| m.len() == 16)
                    .ok_or(format!("fiducial {id}: a 4x4 transform"))?;
                Ok(LayoutTag {
                    id: id as u32,
                    position: [m[3], m[7], m[11]],
                    rotation: [[m[0], m[1], m[2]], [m[4], m[5], m[6]], [m[8], m[9], m[10]]],
                    size: num(f, "size").map_or(tag_size, |mm| mm / 1000.0),
                })
            })
            .collect();
    }
    Err(format!(
        "{}: neither a WPILib layout (tags) nor a Limelight map (fiducials)",
        path.display()
    ))
}

/// A calibration file (HeliOS's: `width`, `height`, `model` `fisheye` or `pinhole`, `fx`,
/// `fy`, `cx`, `cy`, `distortion` with `k1`..`k6`, `p1`, `p2`; at the top or under
/// `calibration`).
pub(crate) fn calibration(path: &Path) -> Result<Lens, String> {
    let json = read_json(path)?;
    let c = json.get("calibration").unwrap_or(&json);
    let get = |k: &str| num(c, k).ok_or(format!("{}: no {k}", path.display()));
    let d = c.get("distortion");
    let k = |name: &str| d.and_then(|d| num(d, name)).unwrap_or(0.0);
    let model = match c.get("model").and_then(Value::as_str).unwrap_or("pinhole") {
        "fisheye" => Model::Fisheye([k("k1"), k("k2"), k("k3"), k("k4")]),
        _ => Model::Pinhole {
            k: [k("k1"), k("k2"), k("k3"), k("k4"), k("k5"), k("k6")],
            p: [k("p1"), k("p2")],
        },
    };
    Ok(Lens {
        width: get("width")? as u32,
        height: get("height")? as u32,
        fx: get("fx")?,
        fy: get("fy")?,
        cx: get("cx")?,
        cy: get("cy")?,
        model,
    })
}

/// A pose from `x`, `y`, `z` (m) and `roll`, `pitch`, `yaw` (degrees).
fn pose_of(v: &Value) -> Pose {
    let n = |k| num(v, k).unwrap_or(0.0);
    Pose::rpy_degrees([n("x"), n("y"), n("z")], [n("roll"), n("pitch"), n("yaw")])
}

/// The camera's path: one pose, or poses at times (s) followed in a loop.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CameraPath {
    Still(Pose),
    Moving(Vec<(f64, Pose)>),
}

impl CameraPath {
    /// The camera's pose at `t` seconds.
    pub(crate) fn at(&self, t: f64) -> Pose {
        match self {
            Self::Still(p) => *p,
            Self::Moving(keys) => {
                let end = keys.last().map_or(0.0, |k| k.0);
                let t = if end > 0.0 { t.rem_euclid(end) } else { 0.0 };
                let i = keys.iter().rposition(|k| k.0 <= t).unwrap_or(0);
                match keys.get(i + 1) {
                    Some(next) => {
                        let span = (next.0 - keys[i].0).max(1e-9);
                        keys[i].1.lerp(&next.1, (t - keys[i].0) / span)
                    }
                    None => keys[i].1,
                }
            }
        }
    }

    pub(crate) fn moves(&self) -> bool {
        matches!(self, Self::Moving(k) if k.len() > 1)
    }
}

/// A camera file: `{"pose": {...}}` or `{"trajectory": [{"t": 0, "x": ..., "yaw": ...}, ...]}`.
pub(crate) fn camera(path: &Path) -> Result<CameraPath, String> {
    let json = read_json(path)?;
    if let Some(keys) = json.get("trajectory").and_then(Value::as_array) {
        let mut keys: Vec<(f64, Pose)> = keys
            .iter()
            .map(|k| (num(k, "t").unwrap_or(0.0), pose_of(k)))
            .collect();
        keys.sort_by(|a, b| a.0.total_cmp(&b.0));
        if keys.is_empty() {
            return Err(format!("{}: an empty trajectory", path.display()));
        }
        return Ok(CameraPath::Moving(keys));
    }
    Ok(CameraPath::Still(pose_of(
        json.get("pose").unwrap_or(&json),
    )))
}

/// `x:y:z:roll:pitch:yaw` (m, degrees).
pub(crate) fn inline_pose(text: &str) -> Result<Pose, String> {
    let v: Vec<f64> = text
        .split(':')
        .map(|p| {
            p.trim()
                .parse::<f64>()
                .map_err(|_| format!("pose={text}: x:y:z:roll:pitch:yaw"))
        })
        .collect::<Result<_, _>>()?;
    let [x, y, z, roll, pitch, yaw] = v[..] else {
        return Err(format!("pose={text}: x:y:z:roll:pitch:yaw"));
    };
    Ok(Pose::rpy_degrees([x, y, z], [roll, pitch, yaw]))
}
