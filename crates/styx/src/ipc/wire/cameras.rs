//! The cameras a service serves (the list reply), with each camera's modes after the list
//! (a trailer: tag 1, then each camera's modes in list order), which peers that predate it
//! ignore; the protocol version is unchanged.

use styx_core::prelude::*;

use super::{IpcError, KIND_CAMERAS, MAX_CAMERAS, MAX_KEYS, Reader, Writer};

/// Most modes a camera lists.
const MAX_MODES: usize = 64;
const MODES_TAG: u8 = 1;

/// A camera a [`CameraService`](super::super::CameraService) serves.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraInfo {
    /// Its display name (e.g. `ov9782` or `046d:0825`); requests may name it by any part.
    pub name: String,
    /// Identity keys (USB vendor/product, bus path, sensor path); requests may name one.
    pub keys: Vec<String>,
    /// Clients are receiving frames from it.
    pub in_use: bool,
    /// Its modes (empty from services that predate them): a request's size and rate pick one,
    /// `Frames::...().sensor_mode(w, h)` names one.
    pub modes: Vec<CameraMode>,
}

/// A camera mode as a service lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    /// The format it delivers (a processed one, e.g. NV12, or the sensor's raw format).
    pub format: FourCc,
    /// Frame rates it runs at (equal for a fixed rate; 0 when unknown).
    pub min_fps: f32,
    pub max_fps: f32,
    /// The area of the sensor's pixel array it reads (left, top, width, height), when known: a
    /// mode reading less than the whole array is cropped (a narrower field of view).
    pub field: Option<[u32; 4]>,
    /// Binning (pixels summed) and skipping (pixels left out), horizontally and vertically: a
    /// mode smaller than its field without them is scaled.
    pub binning: [u32; 2],
    pub skipping: [u32; 2],
}

pub(in crate::ipc) fn encode_cameras(cameras: &[CameraInfo]) -> Vec<u8> {
    let mut w = Writer::new(KIND_CAMERAS);
    let cameras = &cameras[..cameras.len().min(MAX_CAMERAS)];
    w.u8(cameras.len() as u8);
    for camera in cameras {
        w.text(&camera.name);
        w.u8(camera.keys.len().min(usize::from(MAX_KEYS)) as u8);
        for key in camera.keys.iter().take(usize::from(MAX_KEYS)) {
            w.text(key);
        }
        w.bool(camera.in_use);
    }
    w.u8(MODES_TAG);
    for camera in cameras {
        let modes = &camera.modes[..camera.modes.len().min(MAX_MODES)];
        w.u8(modes.len() as u8);
        for m in modes {
            w.u32(m.width);
            w.u32(m.height);
            w.u32(m.format.to_u32());
            w.u32(m.min_fps.to_bits());
            w.u32(m.max_fps.to_bits());
            w.opt(m.field, |w, f| f.iter().for_each(|&v| w.u32(v)));
            for v in m.binning.iter().chain(&m.skipping) {
                w.u32(*v);
            }
        }
    }
    w.0
}

pub(in crate::ipc) fn read_cameras(r: &mut Reader<'_>) -> Result<Vec<CameraInfo>, IpcError> {
    let count = usize::from(r.u8()?);
    if count > MAX_CAMERAS {
        return Err(IpcError::Malformed("too many cameras"));
    }
    let mut cameras = (0..count)
        .map(|_| {
            let name = r.text()?;
            let count = r.u8()?;
            if count > MAX_KEYS {
                return Err(IpcError::Malformed("too many identity keys"));
            }
            let keys = (0..count)
                .map(|_| r.text())
                .collect::<Result<_, IpcError>>()?;
            Ok(CameraInfo {
                name,
                keys,
                in_use: r.bool()?,
                modes: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, IpcError>>()?;
    if r.0.first() == Some(&MODES_TAG) {
        r.u8()?;
        for camera in &mut cameras {
            let n = usize::from(r.u8()?);
            if n > MAX_MODES {
                return Err(IpcError::Malformed("too many modes"));
            }
            for _ in 0..n {
                let (width, height) = (r.u32()?, r.u32()?);
                let format = FourCc::new(r.u32()?.to_le_bytes());
                let (min_fps, max_fps) = (f32::from_bits(r.u32()?), f32::from_bits(r.u32()?));
                let field = r.opt(|r| Ok([r.u32()?, r.u32()?, r.u32()?, r.u32()?]))?;
                let v = [r.u32()?, r.u32()?, r.u32()?, r.u32()?];
                camera.modes.push(CameraMode {
                    width,
                    height,
                    format,
                    min_fps,
                    max_fps,
                    field,
                    binning: [v[0], v[1]],
                    skipping: [v[2], v[3]],
                });
            }
        }
    }
    Ok(cameras)
}

/// `device`'s modes, for its list entry: each mode of its first backend, with the field and
/// binning a native camera reports per sensor mode (`mode:WxH` properties).
pub(in crate::ipc) fn camera_modes(device: &crate::ProbedDevice) -> Vec<CameraMode> {
    let Some(backend) = device.backends.first() else {
        return Vec::new();
    };
    backend
        .descriptor
        .modes
        .iter()
        .map(|m| {
            let (w, h) = (
                m.format.resolution.width.get(),
                m.format.resolution.height.get(),
            );
            let fps = m.intervals.iter().map(|i| i.fps()).chain(
                m.interval_stepwise
                    .iter()
                    .flat_map(|s| [s.min.fps(), s.max.fps()]),
            );
            let (min_fps, max_fps) =
                fps.fold((f32::MAX, 0f32), |(lo, hi), f| (lo.min(f), hi.max(f)));
            let geometry = backend
                .properties
                .iter()
                .find(|(k, _)| *k == format!("mode:{w}x{h}"))
                .map(|(_, v)| parse_geometry(v));
            let (field, binning, skipping) = geometry.unwrap_or((None, [1, 1], [1, 1]));
            CameraMode {
                width: w,
                height: h,
                format: m.format.code,
                min_fps: if min_fps == f32::MAX { 0.0 } else { min_fps },
                max_fps,
                field,
                binning,
                skipping,
            }
        })
        .collect()
}

/// `crop=l,t,w,h binning=bxb skipping=sxs`.
fn parse_geometry(v: &str) -> (Option<[u32; 4]>, [u32; 2], [u32; 2]) {
    let (mut field, mut binning, mut skipping) = (None, [1, 1], [1, 1]);
    for part in v.split_whitespace() {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let nums: Vec<u32> = v.split([',', 'x']).filter_map(|n| n.parse().ok()).collect();
        match (k, nums.as_slice()) {
            ("crop", &[l, t, w, h]) => field = Some([l, t, w, h]),
            ("binning", &[x, y]) => binning = [x, y],
            ("skipping", &[x, y]) => skipping = [x, y],
            _ => {}
        }
    }
    (field, binning, skipping)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_follow_the_camera_list_and_older_readers_stop_before_them() {
        let mode = CameraMode {
            width: 640,
            height: 400,
            format: FourCc::NV12,
            min_fps: 2.1,
            max_fps: 240.0,
            field: Some([0, 0, 1280, 800]),
            binning: [2, 2],
            skipping: [1, 1],
        };
        let cameras = vec![CameraInfo {
            name: "ov9782".into(),
            keys: vec!["bridge:/dev/v4l-subdev2".into()],
            in_use: true,
            modes: vec![mode.clone()],
        }];
        let bytes = encode_cameras(&cameras);
        let (mut r, kind) = Reader::start(&bytes).unwrap();
        assert_eq!(kind, KIND_CAMERAS);
        assert_eq!(read_cameras(&mut r).unwrap(), cameras);
        assert_eq!(
            parse_geometry("crop=0,0,1280,800 binning=2x2 skipping=1x1"),
            (Some([0, 0, 1280, 800]), [2, 2], [1, 1])
        );
    }
}
