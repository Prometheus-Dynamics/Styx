//! Simulated cameras (feature `sim`): a sensor from its description on a simulated bridge and
//! capture queue, opened, configured and streamed through the same [`NativeCamera`] as a real
//! one, so everything above it (the session and its event thread, the sensor driver and its
//! control schedule with the description's delays and limits, the software ISP and 3A, a
//! camera service) runs as on the device.
//!
//! - **The sensor**: the driver writes its registers (a [`SimBus`]): modes, frame length,
//!   exposure, gain and flips, with the description's ranges, clamping and delays.
//! - **The frames**: a producer thread starts a frame every frame period (the frame length the
//!   control schedule applied), exposes the scene at the frame's exposure and gain, reads the
//!   array out mirrored as the flip registers say (so the frame's colour filter order changes
//!   with them) and writes it in the receiver's format (CSI-2 packed RAW10 for the OV9782).
//! - **Faults** ([`SimFaults`]): frames lost on the link, timing jitter, corrupted frames, a
//!   capture error now and then (the capture restarts), and the sensor disappearing and coming
//!   back.
//!
//! Cameras are listed by [`SimSpec`]s from `STYX_SIM_CAMERAS` or `/etc/styx/sim-cameras` (see
//! [`specs`]) and keyed `sim:<sensor>:<n>`. With no PiSP in their media graph they run the
//! software ISP. See `docs/simulation.md`.

mod bus;
mod rig;
pub mod scene;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use styx_kernel::FourCc;
use styx_kernel::bus::BridgeLocation;
use styx_kernel::media::{
    Entity, EntityFlags, EntityFunction, Link, LinkFlags, Pad, PadFlags, Topology,
};

pub use self::bus::{SimBus, SimPower};
pub(crate) use self::rig::{SimRig, configure, open};
pub use self::scene::{ChartScene, FlatScene, SceneFrame, SimScene};
use crate::discover::CameraInfo;
use crate::error::{NativeError, Result};
use crate::fake::lock;
use crate::graph::GraphMap;
use crate::library::SensorLibrary;
use crate::modes::sensor_modes;
use crate::topology::find_route;

/// Simulated cameras, one spec per entry (`;` or a new line between entries).
pub const SIM_ENV: &str = "STYX_SIM_CAMERAS";
/// The same, from a file (one per line, `#` comments), when [`SIM_ENV`] is not set.
pub const SIM_FILE: &str = "/etc/styx/sim-cameras";
/// The key prefix of simulated cameras.
pub const SIM_PREFIX: &str = "sim:";

/// What a simulated camera sees.
#[derive(Clone, Debug, PartialEq)]
pub enum SimSource {
    /// The test chart ([`ChartScene`]), lit `light` times the default.
    Chart { light: f32 },
    /// An even grey of reflectance `grey`.
    Flat { grey: f32, light: f32 },
    /// A recording of raw frames, looped (read by the loader the application registers with
    /// [`set_recording_loader`]: styx's MCAP and styxrec readers).
    Recording(PathBuf),
}

/// Faults a simulated camera injects.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimFaults {
    /// Probability that a frame is lost on the link (no frame start, no buffer; the sequence
    /// skips it).
    pub drop: f64,
    /// Probability that a frame arrives flagged as corrupted (the receiver's error flag).
    pub corrupt: f64,
    /// Frame start timing jitter: each frame starts up to this much early or late.
    pub jitter: Duration,
    /// A capture error (the queue fails, the capture restarts) every this long while streaming.
    pub fail_every: Option<Duration>,
    /// The sensor disappears every this long while streaming (the capture ends; the camera is
    /// not listed and does not open) ...
    pub unplug_every: Option<Duration>,
    /// ... for this long, then comes back.
    pub unplug_for: Duration,
    /// Seed of the faults and noise (the same seed gives the same faults).
    pub seed: u64,
}

/// A simulated camera.
#[derive(Clone, Debug, PartialEq)]
pub struct SimSpec {
    /// The sensor description's name (e.g. `ov9782`), found in the sensor library.
    pub sensor: String,
    /// Its number among simulated cameras of that sensor (from 0, in spec order).
    pub index: u32,
    pub source: SimSource,
    pub faults: SimFaults,
    /// Sensor noise (on by default; off gives exact frames, for tests).
    pub noise: bool,
}

impl SimSpec {
    /// A camera of `sensor` seeing the test chart.
    pub fn new(sensor: &str) -> Self {
        Self {
            sensor: sensor.to_owned(),
            index: 0,
            source: SimSource::Chart { light: 1.0 },
            faults: SimFaults::default(),
            noise: true,
        }
    }

    /// Its key: `sim:<sensor>:<index>`.
    pub fn key(&self) -> String {
        format!("{SIM_PREFIX}{}:{}", self.sensor, self.index)
    }

    /// Parses `<sensor>[,key=value]...`: `scene=chart|flat`, `light=<f>`, `grey=<f>`,
    /// `recording=<path>`, `noise=on|off`, and the faults `drop=<p>`, `corrupt=<p>`,
    /// `jitter_ms=<f>`, `fail_every_s=<f>`, `unplug_every_s=<f>`, `unplug_for_s=<f>`,
    /// `seed=<n>`.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let mut parts = text.split(',').map(str::trim);
        let sensor = parts
            .next()
            .filter(|s| !s.is_empty() && !s.contains('='))
            .ok_or_else(|| format!("\"{text}\": the sensor name first"))?;
        let mut spec = Self::new(sensor);
        let (mut light, mut grey, mut scene) = (1.0f32, 0.18f32, "chart".to_owned());
        let num = |k: &str, v: &str| {
            v.parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| format!("{k}={v}: a number"))
        };
        let secs = |k: &str, v: &str| num(k, v).map(Duration::from_secs_f64);
        for part in parts.filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("\"{part}\": key=value"))?;
            match k {
                "scene" => scene = v.to_owned(),
                "recording" => spec.source = SimSource::Recording(PathBuf::from(v)),
                "light" => light = num(k, v)? as f32,
                "grey" => grey = num(k, v)? as f32,
                "noise" => spec.noise = !matches!(v, "off" | "0" | "false"),
                "drop" => spec.faults.drop = num(k, v)?.min(1.0),
                "corrupt" => spec.faults.corrupt = num(k, v)?.min(1.0),
                "jitter_ms" => spec.faults.jitter = Duration::from_secs_f64(num(k, v)? / 1e3),
                "fail_every_s" => spec.faults.fail_every = Some(secs(k, v)?),
                "unplug_every_s" => spec.faults.unplug_every = Some(secs(k, v)?),
                "unplug_for_s" => spec.faults.unplug_for = secs(k, v)?,
                "seed" => spec.faults.seed = num(k, v)? as u64,
                _ => return Err(format!("unknown key {k}")),
            }
        }
        if !matches!(spec.source, SimSource::Recording(_)) {
            spec.source = match scene.as_str() {
                "chart" => SimSource::Chart { light },
                "flat" => SimSource::Flat { grey, light },
                other => return Err(format!("scene={other}: chart or flat")),
            };
        }
        if spec.faults.unplug_every.is_some() && spec.faults.unplug_for.is_zero() {
            spec.faults.unplug_for = Duration::from_secs(2);
        }
        Ok(spec)
    }
}

/// Parses a list of specs (`;` or new lines between them, `#` comments), numbering the
/// cameras of each sensor in order.
pub fn parse_specs(text: &str) -> std::result::Result<Vec<SimSpec>, String> {
    let mut specs: Vec<SimSpec> = Vec::new();
    for entry in text
        .split(['\n', ';'])
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
    {
        let mut spec = SimSpec::parse(entry)?;
        spec.index = specs.iter().filter(|s| s.sensor == spec.sensor).count() as u32;
        specs.push(spec);
    }
    Ok(specs)
}

/// Cameras added by the application ([`register`]).
static REGISTERED: Mutex<Vec<SimSpec>> = Mutex::new(Vec::new());

/// The simulated cameras configured ([`SIM_ENV`], else [`SIM_FILE`]; none when neither is
/// set), then those [`register`]ed. An entry that does not parse is skipped (reported on stderr
/// once).
pub fn specs() -> Vec<SimSpec> {
    let text = match std::env::var(SIM_ENV) {
        Ok(t) => t,
        Err(_) => std::fs::read_to_string(SIM_FILE).unwrap_or_default(),
    };
    let mut specs = match parse_specs(&text) {
        Ok(s) => s,
        Err(e) => {
            static WARNED: OnceLock<()> = OnceLock::new();
            WARNED.get_or_init(|| eprintln!("styx: simulated cameras: {e}"));
            Vec::new()
        }
    };
    specs.extend(lock(&REGISTERED).iter().cloned());
    specs
}

/// Adds a simulated camera for this process (as a line of [`SIM_ENV`] would), numbered after
/// the others of its sensor. Returns it, with its key ([`SimSpec::key`]).
pub fn register(mut spec: SimSpec) -> SimSpec {
    let existing = specs();
    let mut registered = lock(&REGISTERED);
    spec.index = existing
        .iter()
        .filter(|s| s.sensor == spec.sensor)
        .map(|s| s.index + 1)
        .max()
        .unwrap_or(0);
    registered.push(spec.clone());
    spec
}

/// Reads a recording of raw frames into a scene (registered by the application).
pub type RecordingLoader = fn(&Path) -> std::result::Result<Arc<dyn SimScene>, String>;

static RECORDING_LOADER: OnceLock<RecordingLoader> = OnceLock::new();

/// Registers how recordings are read (styx does, with its readers). The first one stays.
pub fn set_recording_loader(loader: RecordingLoader) {
    let _ = RECORDING_LOADER.set(loader);
}

pub(crate) fn scene_for(spec: &SimSpec) -> Result<Arc<dyn SimScene>> {
    Ok(match &spec.source {
        SimSource::Chart { light } => Arc::new(ChartScene::new(*light)),
        SimSource::Flat { grey, light } => Arc::new(FlatScene::new(*grey, *light)),
        SimSource::Recording(path) => {
            let loader = RECORDING_LOADER.get().ok_or_else(|| {
                NativeError::InvalidConfig(
                    "simulated camera recordings need a reader (styx with replay)".into(),
                )
            })?;
            loader(path).map_err(|e| {
                NativeError::InvalidConfig(format!("recording {}: {e}", path.display()))
            })?
        }
    })
}

/// Cameras that disappeared (a fault): their key and when they come back.
static ABSENT: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());

/// Whether the camera `key` is there now.
pub fn present(key: &str) -> bool {
    let mut absent = lock(&ABSENT);
    let now = Instant::now();
    absent.retain(|(_, back)| *back > now);
    !absent.iter().any(|(k, _)| k == key)
}

pub(crate) fn unplug(key: &str, for_: Duration) {
    lock(&ABSENT).push((key.to_owned(), Instant::now() + for_));
}

/// Whether `key` names a simulated camera.
pub fn is_sim_key(key: &str) -> bool {
    key.starts_with(SIM_PREFIX)
}

/// Entities of a simulated camera's media graph.
const SENSOR: u32 = 1;
const RECEIVER: u32 = 2;
const NODE: u32 = 3;

/// A media graph as a CSI-2 receiver with one raw node reports it: sensor → `csi2` → node.
fn sim_topology(sensor: &str) -> Topology {
    let ent = |id, name: &str, f| Entity {
        id,
        name: name.into(),
        function: f,
        flags: EntityFlags(0),
    };
    let pad = |id, entity_id, index, source: bool| Pad {
        id,
        entity_id,
        flags: if source {
            PadFlags::SOURCE
        } else {
            PadFlags::SINK
        },
        index,
    };
    let fixed = LinkFlags::ENABLED | LinkFlags::IMMUTABLE;
    Topology {
        entities: vec![
            ent(SENSOR, sensor, EntityFunction::CAM_SENSOR),
            ent(RECEIVER, "csi2", EntityFunction::VID_IF_BRIDGE),
            ent(NODE, "sim-csi2_ch0", EntityFunction::IO_V4L),
        ],
        pads: vec![
            pad(11, SENSOR, 0, true),
            pad(21, RECEIVER, 0, false),
            pad(22, RECEIVER, 1, true),
            pad(31, NODE, 0, false),
        ],
        links: vec![
            Link {
                id: 101,
                source_id: 11,
                sink_id: 21,
                flags: fixed,
            },
            Link {
                id: 102,
                source_id: 22,
                sink_id: 31,
                flags: fixed,
            },
        ],
        ..Default::default()
    }
}

/// The camera a spec describes, as discovery lists it.
pub fn camera_info(spec: &SimSpec, library: &SensorLibrary) -> Result<CameraInfo> {
    let (description, source) = library.find_with_source(&spec.sensor)?;
    let key = spec.key();
    let name = format!("{} {key}", description.sensor.name);
    let topology = sim_topology(&name);
    let route = find_route(&topology, SENSOR)?;
    let modes = sensor_modes(&description);
    let mut raw_formats: Vec<(u32, Vec<FourCc>)> = Vec::new();
    for m in &modes {
        if !raw_formats.iter().any(|(c, _)| *c == m.code) {
            let f = crate::formats::memory_formats(m.code)
                .iter()
                .map(|f| f.fourcc)
                .collect();
            raw_formats.push((m.code, f));
        }
    }
    let clock = description
        .sensor
        .clocks
        .values()
        .next()
        .copied()
        .unwrap_or(0);
    let mut info = CameraInfo {
        key: key.clone(),
        location: BridgeLocation {
            subdev: PathBuf::from(&key),
            sensor_name: description.sensor.name.clone(),
            i2c_bus: None,
            i2c_address: None,
            clock_frequency: u64::from(clock),
        },
        description,
        description_source: format!("{source} (simulated)"),
        modes,
        media: PathBuf::new(),
        topology,
        route,
        raw_formats,
        graph: GraphMap::empty(),
        kernel: None,
        lens: None,
    };
    info.graph = crate::discover::graph_for(&info);
    Ok(info)
}

/// The simulated cameras configured and present.
pub fn cameras(library: &SensorLibrary) -> (Vec<CameraInfo>, Vec<NativeError>) {
    let (mut found, mut errors) = (Vec::new(), Vec::new());
    for spec in specs().iter().filter(|s| present(&s.key())) {
        match camera_info(spec, library) {
            Ok(info) => found.push(info),
            Err(e) => errors.push(e),
        }
    }
    (found, errors)
}

/// The simulated camera `key`, if configured and present.
pub fn find(key: &str, library: &SensorLibrary) -> Result<CameraInfo> {
    let spec = specs()
        .into_iter()
        .find(|s| s.key() == key)
        .ok_or_else(|| NativeError::Topology(format!("no simulated camera {key}")))?;
    if !present(key) {
        return Err(NativeError::Topology(format!("{key} is unplugged")));
    }
    camera_info(&spec, library)
}

/// The spec of the simulated camera `key`.
pub(crate) fn spec_of(key: &str) -> Option<SimSpec> {
    specs().into_iter().find(|s| s.key() == key)
}

#[cfg(test)]
mod tests;
