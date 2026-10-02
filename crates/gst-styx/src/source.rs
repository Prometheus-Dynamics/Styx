//! Where frames come from: a camera opened in this process through the Styx planner, or a
//! Styx camera service (`styx::ipc`) in another process.

use std::time::Duration;

use styx::ipc::FrameClient;
use styx::planner::{PlannedFrames, plan_frames};
use styx::prelude::*;

use crate::caps::Negotiated;

/// The camera named `selector` (`None` = the first one): its display name, part of it, or one
/// of its identity keys, or a backend property such as a V4L2 node (`/dev/video0`). `virtual` (or
/// `virtual:WIDTHxHEIGHT[:FOURCC]`) is Styx's synthetic test camera.
pub fn find_camera(selector: Option<&str>) -> Result<ProbedDevice, String> {
    if let Some(spec) = selector.and_then(|s| s.strip_prefix("virtual")) {
        return virtual_camera(spec);
    }
    let devices = probe_all();
    let Some(selector) = selector.filter(|s| !s.is_empty()) else {
        return devices
            .into_iter()
            .next()
            .ok_or_else(|| "no cameras found".to_string());
    };
    pick(devices, selector).ok_or_else(|| format!("no camera matches \"{selector}\""))
}

/// The best match for `selector`: an exact key or name before a partial name.
pub fn pick(devices: Vec<ProbedDevice>, selector: &str) -> Option<ProbedDevice> {
    let lower = selector.to_ascii_lowercase();
    // Exact: the name, an identity key, or a backend property such as a V4L2 node's path.
    let exact = |d: &ProbedDevice| {
        d.identity.display == selector
            || d.identity.keys.iter().any(|k| k == selector)
            || d.backends
                .iter()
                .any(|b| b.properties.iter().any(|(_, v)| v == selector))
    };
    let partial = |d: &ProbedDevice| {
        d.identity.display.to_ascii_lowercase().contains(&lower)
            || d.identity
                .keys
                .iter()
                .any(|k| k.to_ascii_lowercase().contains(&lower))
    };
    let index = devices
        .iter()
        .position(exact)
        .or_else(|| devices.iter().position(partial))?;
    devices.into_iter().nth(index)
}

fn virtual_camera(spec: &str) -> Result<ProbedDevice, String> {
    let mut config = VirtualSourceConfig::new().name("virtual").fps(30);
    if let Some(rest) = spec.strip_prefix(':') {
        let (size, format) = rest.split_once(':').unwrap_or((rest, ""));
        let (w, h) = size
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .ok_or_else(|| format!("bad virtual camera size \"{size}\" (want WIDTHxHEIGHT)"))?;
        config = config.resolution(w, h);
        if !format.is_empty() {
            let bytes: [u8; 4] = format
                .as_bytes()
                .try_into()
                .map_err(|_| format!("bad virtual camera format \"{format}\" (want a fourcc)"))?;
            config = config.format(FourCc::new(bytes));
        }
    } else if !spec.is_empty() {
        return Err(format!("unknown camera \"virtual{spec}\""));
    }
    Ok(CaptureRequest::virtual_source(config).into_device())
}

/// A running stream of frames.
pub enum Stream {
    Local(Box<PlannedFrames>),
    Service(Box<FrameClient>),
}

/// What to start a stream with.
pub struct StreamOptions<'a> {
    pub service: Option<&'a str>,
    pub camera: Option<&'a str>,
    pub priority: Priority,
    pub queue_depth: Option<usize>,
}

impl Stream {
    /// Start delivering frames that match `caps`. Returns the stream and a description of what
    /// was planned.
    pub fn start(
        device: Option<&ProbedDevice>,
        caps: &Negotiated,
        options: &StreamOptions<'_>,
    ) -> Result<(Self, String), String> {
        if let Some(path) = options.service {
            let req = with_options(caps.requirements(false), options);
            let client = match options.camera.filter(|c| !c.is_empty()) {
                Some(camera) => FrameClient::request_camera(path, camera, &req),
                None => FrameClient::request(path, &req),
            }
            .map_err(|e| format!("camera service {path}: {e}"))?
            .reconnecting();
            let plan = client.plan().unwrap_or_default();
            return Ok((Self::Service(Box::new(client)), plan));
        }
        let device = device.ok_or("no camera")?;
        let req = with_options(caps.requirements(true), options);
        let mut plan = plan_frames(device, &req).map_err(|e| e.to_string())?;
        if plan.output_resolution() != (caps.width, caps.height) {
            return Err(format!(
                "the camera delivers {}x{}, not {}x{}",
                plan.output_resolution().0,
                plan.output_resolution().1,
                caps.width,
                caps.height
            ));
        }
        // Run at the negotiated rate when the mode offers it (the planner picks the fastest).
        if let Some((num, den)) = caps.fps
            && let Some(interval) = plan.mode.intervals.iter().copied().find(|i| {
                i64::from(i.denominator.get()) * i64::from(den)
                    == i64::from(i.numerator.get()) * i64::from(num)
            })
        {
            plan.interval = Some(interval);
        }
        let description = plan.to_string();
        let frames = plan.start().map_err(|e| e.to_string())?;
        Ok((Self::Local(Box::new(frames)), description))
    }

    /// The next frame, waiting up to `wait`; `Err` when the capture failed (and why).
    pub fn next(&mut self, wait: Duration) -> Result<RecvOutcome<FrameLease>, String> {
        match self {
            Self::Local(frames) => match frames.pipeline() {
                Some(pipeline) => pipeline
                    .next_blocking_result(wait)
                    .map_err(|err| err.to_string()),
                None => Ok(frames.next_frame(wait)),
            },
            Self::Service(client) => Ok(client.recv(wait)),
        }
    }

    /// Set a camera control (in-process cameras only).
    pub fn set_control(&mut self, id: ControlId, value: ControlValue) -> Result<(), String> {
        match self {
            Self::Local(frames) => frames
                .pipeline()
                .ok_or("no capture to control")?
                .capture()
                .set_control(id, value)
                .map_err(|e| e.to_string()),
            Self::Service(_) => Err("controls are not available through a camera service".into()),
        }
    }

    pub fn stop(self) {
        match self {
            Self::Local(frames) => frames.stop(),
            Self::Service(client) => drop(client),
        }
    }
}

fn with_options(req: FrameRequirements, options: &StreamOptions<'_>) -> FrameRequirements {
    let mut overrides = req.overrides.clone();
    overrides.queue_depth = options.queue_depth;
    req.priority(options.priority).overrides(overrides)
}

/// `name` as a control key: lower case, runs of other characters as `_` (V4L2's
/// "Exposure Time, Absolute" is `exposure_time_absolute`), as v4l2src's `extra-controls`.
pub fn control_key(name: &str) -> String {
    let mut key = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            key.push(c.to_ascii_lowercase());
        } else if !key.ends_with('_') {
            key.push('_');
        }
    }
    key.trim_matches('_').to_string()
}

/// Controls from an `extra-controls` structure, resolved against the camera's controls.
pub fn resolve_controls(
    device: &ProbedDevice,
    controls: &gst::StructureRef,
) -> Vec<Result<(ControlId, ControlValue), String>> {
    let metas: Vec<&ControlMeta> = device
        .backends
        .iter()
        .flat_map(|b| b.descriptor.controls.iter())
        .collect();
    controls
        .iter()
        .map(|(field, value)| {
            let key = control_key(field);
            let meta = metas
                .iter()
                .find(|m| control_key(&m.name) == key)
                .ok_or_else(|| format!("camera has no control \"{field}\""))?;
            let value = control_value(meta.kind, value)
                .ok_or_else(|| format!("control \"{field}\": unsupported value {value:?}"))?;
            Ok((meta.id, value))
        })
        .collect()
}

fn control_value(kind: ControlKind, value: &gst::glib::SendValue) -> Option<ControlValue> {
    let int = value
        .get::<i32>()
        .ok()
        .or_else(|| value.get::<u32>().ok().map(|v| v as i32))
        .or_else(|| value.get::<i64>().ok().map(|v| v as i32))
        .or_else(|| value.get::<bool>().ok().map(i32::from))
        .or_else(|| value.get::<f64>().ok().map(|v| v.round() as i32));
    let float = value
        .get::<f64>()
        .ok()
        .or_else(|| value.get::<f32>().ok().map(f64::from))
        .or_else(|| int.map(f64::from));
    Some(match kind {
        ControlKind::Bool => ControlValue::Bool(int? != 0),
        ControlKind::Int => ControlValue::Int(int?),
        ControlKind::Uint | ControlKind::Menu | ControlKind::IntMenu => {
            ControlValue::Uint(u32::try_from(int?).ok()?)
        }
        ControlKind::Float => ControlValue::Float(float? as f32),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_keys_match_v4l2src_style() {
        assert_eq!(
            control_key("Exposure Time, Absolute"),
            "exposure_time_absolute"
        );
        assert_eq!(control_key("brightness"), "brightness");
        assert_eq!(
            control_key("White Balance Temperature, Auto"),
            "white_balance_temperature_auto"
        );
    }

    #[test]
    fn picks_exact_before_partial() {
        let a = virtual_camera("").unwrap();
        let mut b = a.clone();
        b.identity.display = "virtual camera two".into();
        b.identity.keys = vec!["usb:1-2".into()];
        b.backends[0].properties = vec![("path".into(), "/dev/video9".into())];
        let picked = pick(vec![a.clone(), b.clone()], "/dev/video9").unwrap();
        assert_eq!(picked.identity.display, "virtual camera two");
        let picked = pick(vec![b, a], "virtual").unwrap();
        assert_eq!(picked.identity.display, "virtual");
        assert!(pick(vec![], "x").is_none());
    }

    #[test]
    fn virtual_sizes_parse() {
        let device = virtual_camera(":320x240").unwrap();
        let res = device.backends[0].descriptor.modes[0].format.resolution;
        assert_eq!((res.width.get(), res.height.get()), (320, 240));
        assert!(virtual_camera(":big").is_err());
        assert!(virtual_camera("x").is_err());
    }

    #[test]
    fn controls_resolve_by_key_and_kind() {
        gst::init().unwrap();
        let mut device = virtual_camera("").unwrap();
        device.backends[0].descriptor.controls.push(ControlMeta {
            id: ControlId(7),
            name: "Exposure Time, Absolute".into(),
            kind: ControlKind::Int,
            access: Access::ReadWrite,
            min: ControlValue::Int(1),
            max: ControlValue::Int(1000),
            default: ControlValue::Int(100),
            step: None,
            menu: None,
            metadata: Default::default(),
        });
        let s = gst::Structure::builder("controls")
            .field("exposure_time_absolute", 250)
            .field("nope", 1)
            .build();
        let resolved = resolve_controls(&device, &s);
        assert_eq!(
            resolved[0].as_ref().unwrap(),
            &(ControlId(7), ControlValue::Int(250))
        );
        assert!(resolved[1].is_err());
    }
}
