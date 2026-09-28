//! The device graph of a bridged sensor, from the receiver's media topology and the sensor
//! description.
//!
//! Every entity of the media device becomes a node (the bridge a `Sensor`, the CSI-2 receiver
//! a `Receiver`, video nodes `Sink`s, ISP blocks `IspStage`s), every pad a port with the same
//! index, every data link a link with its state. The sensor's port lists each mode with its
//! exact frame interval range; the raw sink lists the pixel formats the receiver writes.

use styx_graph::{
    BusCode, Capabilities, DeviceGraph, FormatCaps, FourCc, IntervalRange, LinkFlags,
    MemoryDomains, Node, NodeId, NodeKind, Port, PortPurpose, ReceiverKind, SizeRange,
};
use styx_graph::{Fraction, GraphPath};
use styx_kernel::media::{EntityFunction, LinkFlags as MediaLinkFlags, PadFlags, Topology};

use crate::formats;
use crate::modes::SensorMode;
use crate::topology::RawRoute;

/// What the graph is built from.
#[derive(Clone, Debug)]
pub struct GraphSpec<'a> {
    /// The receiver's media topology.
    pub topology: &'a Topology,
    /// The raw route from the bridge.
    pub route: &'a RawRoute,
    /// The sensor's modes.
    pub modes: &'a [SensorMode],
    /// Pixel formats the raw node offers per bus code (as `VIDIOC_ENUM_FMT` with an mbus code
    /// reports them); codes missing here use [`formats::memory_formats`].
    pub raw_formats: &'a [(u32, Vec<styx_kernel::FourCc>)],
    /// Extra properties for the sensor node.
    pub sensor_properties: Vec<(String, String)>,
}

/// A device graph and which media entity each node stands for.
#[derive(Clone, Debug)]
pub struct GraphMap {
    /// The graph.
    pub graph: DeviceGraph,
    nodes: Vec<(u32, NodeId)>,
    sensor: u32,
    raw_node: u32,
}

impl GraphMap {
    /// A placeholder before the graph is built.
    pub(crate) fn empty() -> Self {
        GraphMap {
            graph: DeviceGraph::new(),
            nodes: Vec::new(),
            sensor: 0,
            raw_node: 0,
        }
    }

    /// The node of a media entity.
    pub fn node_of(&self, entity: u32) -> Option<NodeId> {
        self.nodes
            .iter()
            .find(|(e, _)| *e == entity)
            .map(|(_, n)| *n)
    }

    /// The media entity behind a node.
    pub fn entity_of(&self, node: NodeId) -> Option<u32> {
        self.nodes.iter().find(|(_, n)| *n == node).map(|(e, _)| *e)
    }

    /// The sensor node.
    pub fn sensor(&self) -> NodeId {
        self.node_of(self.sensor)
            .expect("the sensor is in the graph")
    }

    /// The raw sink node.
    pub fn raw_sink(&self) -> NodeId {
        self.node_of(self.raw_node)
            .expect("the raw node is in the graph")
    }

    /// The pixel path from the sensor to the raw sink.
    pub fn raw_path(&self) -> Option<GraphPath> {
        let sink = self.raw_sink();
        self.graph
            .output_paths(self.sensor())
            .into_iter()
            .find(|p| p.sink() == sink && p.nodes().len() == 3)
    }
}

fn purpose_for_name(name: &str) -> Option<PortPurpose> {
    let n = name.to_ascii_lowercase();
    if n.contains("stats") {
        Some(PortPurpose::Stats)
    } else if n.contains("config") || n.contains("params") {
        Some(PortPurpose::Params)
    } else if n.contains("embedded") || n.contains("meta") {
        Some(PortPurpose::Metadata)
    } else {
        None
    }
}

fn kind_of(topo: &Topology, entity: u32, route: &RawRoute) -> NodeKind {
    let Some(e) = topo.entity(entity) else {
        return NodeKind::Other("unknown".into());
    };
    let name = e.name.to_ascii_lowercase();
    if entity == route.sensor || e.function == EntityFunction::CAM_SENSOR {
        return NodeKind::Sensor;
    }
    match e.function {
        EntityFunction::VID_IF_BRIDGE if name.contains("csi") => {
            NodeKind::Receiver(ReceiverKind::Csi2)
        }
        EntityFunction::VID_IF_BRIDGE => NodeKind::Receiver(ReceiverKind::Other(e.name.clone())),
        EntityFunction::PROC_VIDEO_ISP => NodeKind::IspStage,
        EntityFunction::PROC_VIDEO_SCALER if name.contains("isp") => NodeKind::IspStage,
        EntityFunction::PROC_VIDEO_SCALER => NodeKind::Scaler,
        EntityFunction::IO_V4L => {
            let has_sink = topo
                .pads_of(entity)
                .iter()
                .any(|p| p.flags.contains(PadFlags::SINK));
            if has_sink {
                NodeKind::Sink
            } else {
                NodeKind::Other("memory-source".into())
            }
        }
        f => NodeKind::Other(f.name().unwrap_or("unknown").to_ascii_lowercase()),
    }
}

fn sensor_caps(modes: &[SensorMode]) -> Capabilities {
    modes.iter().fold(Capabilities::new(), |caps, m| {
        caps.format(
            FormatCaps::new(BusCode(m.code))
                .size(SizeRange::discrete(m.width, m.height))
                .interval(IntervalRange::Stepwise {
                    min: m.min_interval,
                    max: m.max_interval,
                    step: Fraction::new(0, 1),
                }),
        )
    })
}

fn codes(modes: &[SensorMode]) -> Vec<u32> {
    let mut codes: Vec<u32> = modes.iter().map(|m| m.code).collect();
    codes.sort_unstable();
    codes.dedup();
    codes
}

fn bus_caps(modes: &[SensorMode]) -> Capabilities {
    codes(modes)
        .into_iter()
        .fold(Capabilities::new(), |caps, c| {
            caps.format(FormatCaps::new(BusCode(c)))
        })
}

/// The pixel formats the raw node writes for `code`.
pub fn raw_formats_for(code: u32, offered: &[(u32, Vec<styx_kernel::FourCc>)]) -> Vec<FourCc> {
    match offered.iter().find(|(c, f)| *c == code && !f.is_empty()) {
        Some((_, f)) => f.iter().map(|f| FourCc(f.0)).collect(),
        None => formats::memory_formats(code)
            .iter()
            .map(|f| FourCc(f.fourcc.0))
            .collect(),
    }
}

fn raw_sink_caps(
    modes: &[SensorMode],
    offered: &[(u32, Vec<styx_kernel::FourCc>)],
) -> Capabilities {
    let mut caps = Capabilities::new().memory(MemoryDomains::CPU | MemoryDomains::DMABUF);
    for code in codes(modes) {
        for fourcc in raw_formats_for(code, offered) {
            let fc = modes
                .iter()
                .filter(|m| m.code == code)
                .fold(FormatCaps::new(fourcc), |f, m| {
                    f.size(SizeRange::discrete(m.width, m.height))
                });
            caps = caps.format(fc);
        }
    }
    caps
}

/// Builds the graph.
pub fn build_graph(spec: &GraphSpec<'_>) -> GraphMap {
    let topo = spec.topology;
    let route = spec.route;
    let mut graph = DeviceGraph::new();
    let mut nodes = Vec::new();
    let data_links = topo.data_links();
    // A pad's purpose follows the video node on the other end of its links.
    let linked_purpose = |entity: u32, index: u32| {
        data_links
            .iter()
            .filter_map(|l| {
                if l.source.entity == entity && l.source.index == index {
                    Some(l.sink.entity)
                } else if l.sink.entity == entity && l.sink.index == index {
                    Some(l.source.entity)
                } else {
                    None
                }
            })
            .filter_map(|other| topo.entity(other))
            .filter(|e| e.function == EntityFunction::IO_V4L)
            .find_map(|e| purpose_for_name(&e.name))
    };
    for e in &topo.entities {
        let kind = kind_of(topo, e.id, route);
        let mut node = Node::new(e.name.clone(), kind.clone())
            .property("entity", e.id.to_string())
            .property(
                "function",
                e.function.name().unwrap_or("unknown").to_ascii_lowercase(),
            );
        if let Some(path) = topo.devnode_path(e.id) {
            node = node.device(path.display().to_string());
        }
        if e.id == route.sensor {
            for (k, v) in &spec.sensor_properties {
                node = node.property(k.clone(), v.clone());
            }
        }
        let own_purpose = purpose_for_name(&e.name).filter(|_| kind == NodeKind::Sink);
        let pads = topo.pads_of(e.id);
        let count = pads.iter().map(|p| p.index + 1).max().unwrap_or(0);
        for index in 0..count {
            let pad = pads.iter().find(|p| p.index == index);
            let source = pad.is_some_and(|p| p.flags.contains(PadFlags::SOURCE));
            let caps = if e.id == route.sensor && index == route.sensor_pad {
                sensor_caps(spec.modes)
            } else if e.id == route.receiver
                && (index == route.receiver_sink || index == route.receiver_source)
            {
                bus_caps(spec.modes)
            } else if e.id == route.node {
                raw_sink_caps(spec.modes, spec.raw_formats)
            } else {
                Capabilities::new()
            };
            let name = match (&kind, source) {
                (NodeKind::Sink, false) => "in".to_owned(),
                (NodeKind::Sensor, true) if count == 1 => "out".to_owned(),
                _ => format!("pad{index}"),
            };
            let port = if source {
                Port::output(name, caps)
            } else {
                Port::input(name, caps)
            };
            let purpose = own_purpose
                .or_else(|| linked_purpose(e.id, index))
                .unwrap_or_default();
            node = node.port(port.purpose(purpose));
        }
        nodes.push((e.id, graph.add_node(node)));
    }
    let node_of = |entity: u32| nodes.iter().find(|(e, _)| *e == entity).map(|(_, n)| *n);
    for l in &data_links {
        let (Some(from), Some(to)) = (node_of(l.source.entity), node_of(l.sink.entity)) else {
            continue;
        };
        let flags = if l.flags.contains(MediaLinkFlags::IMMUTABLE) {
            LinkFlags::IMMUTABLE
        } else if l.flags.contains(MediaLinkFlags::ENABLED) {
            LinkFlags::ENABLED
        } else {
            LinkFlags::DISABLED
        };
        // Links the graph model cannot express (wrong directions) are left out.
        let _ = graph.link(
            from.port(l.source.index as u16),
            to.port(l.sink.index as u16),
            flags,
        );
    }
    GraphMap {
        graph,
        nodes,
        sensor: route.sensor,
        raw_node: route.node,
    }
}

#[cfg(test)]
mod tests {
    use styx_graph::{FormatCode, Size, StreamConfig};
    use styx_sensor::SensorDescription;

    use super::*;
    use crate::modes::sensor_modes;
    use crate::topology::{self, testing::cm5_topology};

    fn built() -> GraphMap {
        let desc = SensorDescription::from_file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sensor/sensors/ov9782.toml"
        ))
        .unwrap();
        let topo = cm5_topology("ov9782 styx-sensor-bridge-cam0");
        let route = topology::find_route(&topo, 16).unwrap();
        let modes = sensor_modes(&desc);
        build_graph(&GraphSpec {
            topology: &topo,
            route: &route,
            modes: &modes,
            raw_formats: &[(0x3007, vec![styx_kernel::FourCc::new(b"pBAA")])],
            sensor_properties: vec![("bridge".into(), "/dev/v4l-subdev2".into())],
        })
    }

    #[test]
    fn the_cm5_graph_has_the_expected_shape() {
        let m = built();
        let g = &m.graph;
        g.validate().unwrap();
        let sensor = g.node(m.sensor()).unwrap();
        assert_eq!(sensor.kind, NodeKind::Sensor);
        assert!(
            sensor
                .properties
                .contains(&("bridge".into(), "/dev/v4l-subdev2".into()))
        );
        assert_eq!(
            g.find("csi2").unwrap().kind,
            NodeKind::Receiver(ReceiverKind::Csi2)
        );
        assert_eq!(g.find("pisp-fe").unwrap().kind, NodeKind::IspStage);
        assert_eq!(
            g.find("rp1-cfe-fe_config").unwrap().kind,
            NodeKind::Other("memory-source".into())
        );
        let csi = g.find("csi2").unwrap();
        assert_eq!(csi.ports.len(), 6);
        assert_eq!(csi.ports[5].purpose, PortPurpose::Metadata);
        let stats = g.find("pisp-fe").unwrap();
        assert_eq!(stats.ports[4].purpose, PortPurpose::Stats);
        assert_eq!(stats.ports[1].purpose, PortPurpose::Params);
        // Paths: raw ch0, the embedded node, and the front end image and stats outputs.
        let paths: Vec<String> = g.all_paths().iter().map(|p| p.describe(g)).collect();
        assert!(
            paths.contains(&"ov9782 styx-sensor-bridge-cam0 -> csi2 -> rp1-cfe-csi2_ch0".into())
        );
        assert_eq!(paths.len(), 4, "{paths:?}");
    }

    #[test]
    fn the_raw_path_takes_the_sensor_modes_at_exact_rates() {
        let m = built();
        let g = &m.graph;
        let path = m.raw_path().unwrap();
        assert_eq!(path.sink(), m.raw_sink());
        let pbaa = FourCc::new(b"pBAA");
        let sink = g.port(path.output()).unwrap();
        assert!(
            sink.caps
                .accepts(FormatCode::Memory(pbaa), Size::new(1280, 800), None)
        );
        assert!(
            sink.caps
                .accepts(FormatCode::Memory(pbaa), Size::new(640, 400), None)
        );
        // raw8 falls back to the table.
        assert!(
            sink.caps
                .find(FormatCode::Memory(FourCc::new(b"BA81")))
                .is_some()
        );
        let at = |fps| {
            StreamConfig::new(path.clone(), pbaa, Size::new(1280, 800))
                .interval(Fraction::from_fps(fps))
                .check(g)
        };
        at(30).unwrap();
        at(120).unwrap();
        // Rates are checked per port, not per size: only rates no mode reaches fail here
        // (the 640x400 raw8 mode reaches 325 fps).
        assert!(at(400).is_err());
        // The graph's plan enables the raw link; the kernel plan also disables the front end.
        let plan = g.link_plan(&[&path]).unwrap();
        assert_eq!(plan.enable.len(), 1);
        assert!(plan.disable.is_empty());
    }
}
