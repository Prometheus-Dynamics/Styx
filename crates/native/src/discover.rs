//! Finding bridged cameras: bridges in sysfs, the media graph each belongs to, the matching
//! sensor description, and what the raw node can write.

use std::path::PathBuf;
use std::sync::Arc;

use styx_kernel::FourCc;
use styx_kernel::bus::{BridgeLocation, find_bridges};
use styx_kernel::media::{self, MediaDevice, Topology};
use styx_kernel::v4l2::{BufType, VideoDevice};
use styx_sensor::SensorDescription;

use crate::error::{KernelContext, NativeError, Result};
use crate::graph::{GraphMap, GraphSpec, build_graph};
use crate::library::SensorLibrary;
use crate::modes::{SensorMode, sensor_modes};
use crate::topology::{RawRoute, entity_for_devnode, find_route};

/// A camera behind a sensor bridge, as discovery found it.
#[derive(Clone, Debug)]
pub struct CameraInfo {
    /// Key: `bridge:<subdev path>`, valid until the bridge is rebound (the subdev number can
    /// change then; [`CameraInfo::display_name`] does not).
    pub key: String,
    /// The bridge and what its device tree node says.
    pub location: BridgeLocation,
    /// The sensor description.
    pub description: Arc<SensorDescription>,
    /// Where the description came from.
    pub description_source: String,
    /// The sensor's modes.
    pub modes: Vec<SensorMode>,
    /// The receiver's media device.
    pub media: PathBuf,
    /// Its topology when discovered (link states follow configuration once opened).
    pub topology: Topology,
    /// The raw route from the bridge.
    pub route: RawRoute,
    /// Pixel formats the raw node writes, per bus code.
    pub raw_formats: Vec<(u32, Vec<FourCc>)>,
    /// The device graph.
    pub graph: GraphMap,
}

impl CameraInfo {
    /// Fingerprints identifying the physical camera (`i2c:10-0060`, `native:ov9782`).
    pub fn identity(&self) -> Vec<String> {
        let mut keys = vec![self.key.clone()];
        if let (Some(bus), Some(addr)) = (self.location.i2c_bus, self.location.i2c_address) {
            keys.push(format!("i2c:{bus}-{addr:04x}"));
        }
        keys.push(format!("native:{}", self.location.sensor_name));
        keys
    }

    /// Human-readable name, e.g. `ov9782 (styx bridge i2c 10-0060)`. It names the sensor's I²C
    /// location when the bridge gives one, which stays the same when the bridge is rebound; the
    /// subdev node does not (`/dev/v4l-subdev2` came back as `v4l-subdev3` after the bridge
    /// went down and up under HeliOS), and applications key cameras by this name.
    pub fn display_name(&self) -> String {
        display_name(&self.location)
    }

    /// Properties for listings.
    pub fn properties(&self) -> Vec<(String, String)> {
        let mut p = vec![
            (
                "bridge".to_owned(),
                self.location.subdev.display().to_string(),
            ),
            ("sensor".to_owned(), self.location.sensor_name.clone()),
            ("description".to_owned(), self.description_source.clone()),
            ("media".to_owned(), self.media.display().to_string()),
            ("receiver".to_owned(), self.route.receiver_name.clone()),
            ("raw_node".to_owned(), self.route.node_name.clone()),
        ];
        if let Some(path) = &self.route.node_path {
            p.push(("video".to_owned(), path.display().to_string()));
        }
        if let (Some(bus), Some(addr)) = (self.location.i2c_bus, self.location.i2c_address) {
            p.push(("i2c".to_owned(), format!("{bus}-{addr:04x}")));
        }
        p
    }

    /// Rebuilds the graph from a new topology (after link changes).
    pub(crate) fn rebuild_graph(&mut self, topology: Topology) {
        self.topology = topology;
        self.graph = graph_for(self);
    }
}

fn graph_for(info: &CameraInfo) -> GraphMap {
    build_graph(&GraphSpec {
        topology: &info.topology,
        route: &info.route,
        modes: &info.modes,
        raw_formats: &info.raw_formats,
        sensor_properties: vec![
            ("bridge".into(), info.location.subdev.display().to_string()),
            ("description".into(), info.description_source.clone()),
        ],
    })
}

/// The key of a bridge.
pub fn key_for(location: &BridgeLocation) -> String {
    format!("bridge:{}", location.subdev.display())
}

/// The media device whose graph contains the bridge's subdev, with its topology and the
/// bridge's entity id.
pub fn find_media(location: &BridgeLocation) -> Result<(PathBuf, Topology, u32)> {
    for path in media::list_media_devices() {
        let Ok(dev) = MediaDevice::open_read_only(&path) else {
            continue;
        };
        let Ok(topo) = dev.topology() else { continue };
        if let Some(entity) = entity_for_devnode(&topo, &location.subdev) {
            return Ok((path, topo, entity));
        }
    }
    Err(NativeError::Topology(format!(
        "no media graph contains {}",
        location.subdev.display()
    )))
}

/// What the raw node writes for each bus code of the modes (empty lists where it cannot be
/// asked).
pub fn query_raw_formats(route: &RawRoute, modes: &[SensorMode]) -> Vec<(u32, Vec<FourCc>)> {
    let mut codes: Vec<u32> = modes.iter().map(|m| m.code).collect();
    codes.sort_unstable();
    codes.dedup();
    let video = route
        .node_path
        .as_ref()
        .and_then(|p| VideoDevice::open_read_only(p).ok());
    codes
        .into_iter()
        .map(|code| {
            let offered = video
                .as_ref()
                .and_then(|v| v.formats_for_mbus_code(BufType::VideoCapture, code).ok())
                .map(|f| f.into_iter().map(|d| d.fourcc).collect())
                .unwrap_or_default();
            (code, offered)
        })
        .collect()
}

/// Everything about one bridge.
pub fn discover_bridge(location: &BridgeLocation, library: &SensorLibrary) -> Result<CameraInfo> {
    let (description, description_source) = library.find_with_source(&location.sensor_name)?;
    let (media, topology, entity) = find_media(location)?;
    let route = find_route(&topology, entity)?;
    let modes = sensor_modes(&description);
    let raw_formats = query_raw_formats(&route, &modes);
    let mut info = CameraInfo {
        key: key_for(location),
        location: location.clone(),
        description,
        description_source,
        modes,
        media,
        topology,
        route,
        raw_formats,
        graph: GraphMap::empty(),
    };
    info.graph = graph_for(&info);
    Ok(info)
}

/// Every bridged camera on this system, and the problems met on the way (a bridge without a
/// description, a graph of an unexpected shape).
pub fn discover(library: &SensorLibrary) -> (Vec<CameraInfo>, Vec<NativeError>) {
    let bridges = match find_bridges().step("find sensor bridges") {
        Ok(b) => b,
        Err(e) => return (Vec::new(), vec![e]),
    };
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for loc in &bridges {
        match discover_bridge(loc, library) {
            Ok(info) => found.push(info),
            Err(e) => errors.push(e),
        }
    }
    (found, errors)
}

/// The bridges bound now, by key.
pub fn bridge_keys() -> Vec<(String, BridgeLocation)> {
    find_bridges()
        .unwrap_or_default()
        .into_iter()
        .map(|l| (key_for(&l), l))
        .collect()
}

fn display_name(location: &BridgeLocation) -> String {
    match (location.i2c_bus, location.i2c_address) {
        (Some(bus), Some(addr)) => {
            format!(
                "{} (styx bridge i2c {bus}-{addr:04x})",
                location.sensor_name
            )
        }
        _ => format!(
            "{} (styx bridge {})",
            location.sensor_name,
            location.subdev.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(subdev: &str, i2c: Option<(u32, u16)>) -> BridgeLocation {
        BridgeLocation {
            subdev: PathBuf::from(subdev),
            sensor_name: "ov9782".into(),
            i2c_bus: i2c.map(|(b, _)| b),
            i2c_address: i2c.map(|(_, a)| a),
            clock_frequency: 0,
        }
    }

    #[test]
    fn display_name_survives_a_new_subdev_number() {
        let before = display_name(&location("/dev/v4l-subdev2", Some((10, 0x60))));
        let after = display_name(&location("/dev/v4l-subdev3", Some((10, 0x60))));
        assert_eq!(before, "ov9782 (styx bridge i2c 10-0060)");
        assert_eq!(before, after);
        assert_eq!(
            display_name(&location("/dev/v4l-subdev2", None)),
            "ov9782 (styx bridge /dev/v4l-subdev2)"
        );
    }
}
