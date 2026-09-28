//! The device graph: nodes with ports, and links between ports.

use std::collections::HashSet;
use std::fmt;

use crate::caps::{Capabilities, CostHint};

/// A node in one [`DeviceGraph`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct NodeId(pub u32);

impl NodeId {
    /// The port with `index` on this node.
    pub const fn port(self, index: u16) -> PortRef {
        PortRef { node: self, index }
    }
}

/// A port of a node, by index (a V4L2 pad index).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct PortRef {
    pub node: NodeId,
    pub index: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LinkId(pub u32);

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ReceiverKind {
    Csi2,
    Parallel,
    Usb,
    Network,
    Other(String),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ComputeKind {
    Gpu,
    Npu,
    Cpu,
}

/// What a node is.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum NodeKind {
    /// Produces images: an image sensor, or the camera behind a USB/network receiver.
    Sensor,
    /// Brings pixels into the system: a CSI-2 receiver, a USB video function, a network stream.
    Receiver(ReceiverKind),
    /// A stage of an ISP (front end, back end).
    IspStage,
    Scaler,
    /// An encoder or decoder.
    Codec,
    Compute(ComputeKind),
    /// A buffer queue in memory (a V4L2 video device node): frames land here and can be handed
    /// out, or fed on to a stage that reads memory.
    Sink,
    Other(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Direction {
    /// Data flows into the node (a V4L2 sink pad).
    Input,
    /// Data flows out of the node (a V4L2 source pad).
    Output,
}

/// What travels through a port.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum PortPurpose {
    #[default]
    Pixels,
    /// Statistics for control algorithms (AE, AWB, focus).
    Stats,
    /// Parameters/configuration buffers from userspace.
    Params,
    /// Sensor embedded data and other per-frame metadata.
    Metadata,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Port {
    pub index: u16,
    pub name: String,
    pub direction: Direction,
    pub purpose: PortPurpose,
    pub caps: Capabilities,
}

impl Port {
    /// An input port; its index is its position on the node.
    pub fn input(name: impl Into<String>, caps: Capabilities) -> Self {
        Self::new(name, Direction::Input, caps)
    }

    /// An output port; its index is its position on the node.
    pub fn output(name: impl Into<String>, caps: Capabilities) -> Self {
        Self::new(name, Direction::Output, caps)
    }

    fn new(name: impl Into<String>, direction: Direction, caps: Capabilities) -> Self {
        Port {
            index: 0,
            name: name.into(),
            direction,
            purpose: PortPurpose::Pixels,
            caps,
        }
    }

    pub fn purpose(mut self, purpose: PortPurpose) -> Self {
        self.purpose = purpose;
        self
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Node {
    pub id: NodeId,
    pub name: String,
    pub kind: NodeKind,
    /// The device node that controls it (`/dev/video4`, `/dev/v4l-subdev2`), if any.
    pub device: Option<String>,
    pub ports: Vec<Port>,
    pub cost: CostHint,
    pub properties: Vec<(String, String)>,
}

impl Node {
    pub fn new(name: impl Into<String>, kind: NodeKind) -> Self {
        Node {
            id: NodeId(u32::MAX),
            name: name.into(),
            kind,
            device: None,
            ports: Vec::new(),
            cost: CostHint::FREE,
            properties: Vec::new(),
        }
    }

    /// Add a port; it gets the next index.
    pub fn port(mut self, mut port: Port) -> Self {
        port.index = self.ports.len() as u16;
        self.ports.push(port);
        self
    }

    pub fn device(mut self, path: impl Into<String>) -> Self {
        self.device = Some(path.into());
        self
    }

    pub fn cost(mut self, cost: CostHint) -> Self {
        self.cost = cost;
        self
    }

    pub fn property(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.properties.push((key.into(), value.into()));
        self
    }

    pub fn find_port(&self, name: &str) -> Option<&Port> {
        self.ports.iter().find(|p| p.name == name)
    }

    pub fn inputs(&self) -> impl Iterator<Item = &Port> {
        self.ports
            .iter()
            .filter(|p| p.direction == Direction::Input)
    }

    pub fn outputs(&self) -> impl Iterator<Item = &Port> {
        self.ports
            .iter()
            .filter(|p| p.direction == Direction::Output)
    }
}

/// How data crosses a link.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum LinkMedium {
    /// Streamed between hardware blocks (a media controller link).
    #[default]
    Bus,
    /// Through buffers in memory: one node writes, the next reads (e.g. PiSP front end to back
    /// end, or a capture node to a decoder).
    Memory,
}

/// Link state, as in the media controller.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LinkFlags {
    pub enabled: bool,
    /// Cannot be changed (and is always enabled).
    pub immutable: bool,
}

impl LinkFlags {
    pub const ENABLED: LinkFlags = LinkFlags {
        enabled: true,
        immutable: false,
    };
    pub const DISABLED: LinkFlags = LinkFlags {
        enabled: false,
        immutable: false,
    };
    pub const IMMUTABLE: LinkFlags = LinkFlags {
        enabled: true,
        immutable: true,
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Link {
    pub id: LinkId,
    pub from: PortRef,
    pub to: PortRef,
    pub flags: LinkFlags,
    pub medium: LinkMedium,
}

#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum GraphError {
    #[error("no node {0:?}")]
    UnknownNode(NodeId),
    #[error("no port {0:?}")]
    UnknownPort(PortRef),
    #[error("no link {0:?}")]
    UnknownLink(LinkId),
    #[error("link {from:?} -> {to:?} must go from an output port to an input port")]
    WrongDirection { from: PortRef, to: PortRef },
    #[error("link {0:?} is immutable but disabled")]
    ImmutableDisabled(LinkId),
    #[error("link {0:?} is immutable")]
    Immutable(LinkId),
    #[error("input port {port:?} has {count} enabled links")]
    InputConflict { port: PortRef, count: usize },
    #[error("link {0:?} joins ports with no format in common")]
    IncompatibleFormats(LinkId),
    #[error("duplicate node name {0:?}")]
    DuplicateName(String),
    #[error("path is broken: {0}")]
    BrokenPath(String),
}

/// A device described as nodes, ports and links.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct DeviceGraph {
    nodes: Vec<Node>,
    links: Vec<Link>,
}

impl DeviceGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node and return its id.
    pub fn add_node(&mut self, mut node: Node) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        node.id = id;
        self.nodes.push(node);
        id
    }

    /// Link `from` (an output port) to `to` (an input port) over a bus.
    pub fn link(
        &mut self,
        from: PortRef,
        to: PortRef,
        flags: LinkFlags,
    ) -> Result<LinkId, GraphError> {
        self.add_link(from, to, flags, LinkMedium::Bus)
    }

    /// Link `from` to `to` through buffers in memory.
    pub fn memory_link(
        &mut self,
        from: PortRef,
        to: PortRef,
        flags: LinkFlags,
    ) -> Result<LinkId, GraphError> {
        self.add_link(from, to, flags, LinkMedium::Memory)
    }

    fn add_link(
        &mut self,
        from: PortRef,
        to: PortRef,
        flags: LinkFlags,
        medium: LinkMedium,
    ) -> Result<LinkId, GraphError> {
        let (out, input) = (self.port(from)?, self.port(to)?);
        if out.direction != Direction::Output || input.direction != Direction::Input {
            return Err(GraphError::WrongDirection { from, to });
        }
        let id = LinkId(self.links.len() as u32);
        self.links.push(Link {
            id,
            from,
            to,
            flags,
            medium,
        });
        Ok(id)
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn links(&self) -> &[Link] {
        &self.links
    }

    pub fn node(&self, id: NodeId) -> Result<&Node, GraphError> {
        self.nodes
            .get(id.0 as usize)
            .ok_or(GraphError::UnknownNode(id))
    }

    pub fn port(&self, port: PortRef) -> Result<&Port, GraphError> {
        self.node(port.node)?
            .ports
            .get(port.index as usize)
            .ok_or(GraphError::UnknownPort(port))
    }

    pub fn link_by_id(&self, id: LinkId) -> Result<&Link, GraphError> {
        self.links
            .get(id.0 as usize)
            .ok_or(GraphError::UnknownLink(id))
    }

    pub fn find(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// The port called `port` on the node called `node`.
    pub fn find_port(&self, node: &str, port: &str) -> Option<PortRef> {
        let node = self.find(node)?;
        Some(node.id.port(node.find_port(port)?.index))
    }

    pub fn nodes_of_kind<'a>(&'a self, kind: &'a NodeKind) -> impl Iterator<Item = &'a Node> {
        self.nodes.iter().filter(move |n| &n.kind == kind)
    }

    pub fn sensors(&self) -> impl Iterator<Item = &Node> {
        self.nodes_of_kind(&NodeKind::Sensor)
    }

    /// Links leaving `port`.
    pub fn links_from(&self, port: PortRef) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(move |l| l.from == port)
    }

    /// Links arriving at `port`.
    pub fn links_to(&self, port: PortRef) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(move |l| l.to == port)
    }

    /// Enable or disable a mutable link.
    pub fn set_link(&mut self, id: LinkId, enabled: bool) -> Result<(), GraphError> {
        let link = self
            .links
            .get_mut(id.0 as usize)
            .ok_or(GraphError::UnknownLink(id))?;
        if link.flags.immutable {
            if enabled {
                return Ok(());
            }
            return Err(GraphError::Immutable(id));
        }
        link.flags.enabled = enabled;
        Ok(())
    }

    /// Check that the graph is consistent: unique node names, links joining existing ports in
    /// the right direction with formats in common, immutable links enabled, and at most one
    /// enabled link into each input port.
    pub fn validate(&self) -> Result<(), GraphError> {
        let mut names = HashSet::new();
        for node in &self.nodes {
            if !names.insert(node.name.as_str()) {
                return Err(GraphError::DuplicateName(node.name.clone()));
            }
        }
        for link in &self.links {
            let (out, input) = (self.port(link.from)?, self.port(link.to)?);
            if out.direction != Direction::Output || input.direction != Direction::Input {
                return Err(GraphError::WrongDirection {
                    from: link.from,
                    to: link.to,
                });
            }
            if link.flags.immutable && !link.flags.enabled {
                return Err(GraphError::ImmutableDisabled(link.id));
            }
            if !out.caps.compatible(&input.caps) {
                return Err(GraphError::IncompatibleFormats(link.id));
            }
        }
        for node in &self.nodes {
            for port in node.inputs() {
                let port = node.id.port(port.index);
                let count = self.links_to(port).filter(|l| l.flags.enabled).count();
                if count > 1 {
                    return Err(GraphError::InputConflict { port, count });
                }
            }
        }
        Ok(())
    }
}

impl fmt::Display for DeviceGraph {
    /// One line per link: `ov9782:0 -> csi2:0 [enabled, immutable]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = |p: PortRef| -> String {
            match (self.node(p.node), self.port(p)) {
                (Ok(n), Ok(port)) => format!("{}:{}", n.name, port.name),
                _ => format!("{p:?}"),
            }
        };
        for link in &self.links {
            let mut flags = vec![if link.flags.enabled {
                "enabled"
            } else {
                "disabled"
            }];
            if link.flags.immutable {
                flags.push("immutable");
            }
            if link.medium == LinkMedium::Memory {
                flags.push("memory");
            }
            writeln!(
                f,
                "{} -> {} [{}]",
                name(link.from),
                name(link.to),
                flags.join(", ")
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{BusCode, FormatCaps};

    fn bus(code: BusCode) -> Capabilities {
        Capabilities::new().format(FormatCaps::new(code))
    }

    fn two_nodes() -> (DeviceGraph, NodeId, NodeId) {
        let mut g = DeviceGraph::new();
        let a = g.add_node(
            Node::new("a", NodeKind::Sensor).port(Port::output("out", bus(BusCode::Y8_1X8))),
        );
        let b = g.add_node(
            Node::new("b", NodeKind::Receiver(ReceiverKind::Csi2))
                .port(Port::input("in", bus(BusCode::Y8_1X8)))
                .port(Port::output("out", Capabilities::new())),
        );
        (g, a, b)
    }

    #[test]
    fn builds_and_finds() {
        let (mut g, a, b) = two_nodes();
        let link = g.link(a.port(0), b.port(0), LinkFlags::IMMUTABLE).unwrap();
        assert_eq!(g.find("b").unwrap().id, b);
        assert_eq!(g.find_port("b", "out"), Some(b.port(1)));
        assert_eq!(g.find_port("b", "nope"), None);
        assert_eq!(g.sensors().count(), 1);
        assert_eq!(g.links_from(a.port(0)).count(), 1);
        assert_eq!(g.links_to(b.port(0)).next().unwrap().id, link);
        assert_eq!(g.node(b).unwrap().inputs().count(), 1);
        assert_eq!(g.node(b).unwrap().outputs().count(), 1);
        g.validate().unwrap();
        assert_eq!(g.to_string(), "a:out -> b:in [enabled, immutable]\n");
    }

    #[test]
    fn rejects_bad_links() {
        let (mut g, a, b) = two_nodes();
        assert_eq!(
            g.link(b.port(0), a.port(0), LinkFlags::ENABLED),
            Err(GraphError::WrongDirection {
                from: b.port(0),
                to: a.port(0)
            })
        );
        assert_eq!(
            g.link(a.port(3), b.port(0), LinkFlags::ENABLED),
            Err(GraphError::UnknownPort(a.port(3)))
        );
        assert_eq!(
            g.link(NodeId(9).port(0), b.port(0), LinkFlags::ENABLED),
            Err(GraphError::UnknownNode(NodeId(9)))
        );
        assert!(g.link_by_id(LinkId(0)).is_err());
    }

    #[test]
    fn validation_catches_conflicts() {
        let (mut g, a, b) = two_nodes();
        let second = g.add_node(
            Node::new("c", NodeKind::Sensor).port(Port::output("out", bus(BusCode::Y8_1X8))),
        );
        g.link(a.port(0), b.port(0), LinkFlags::ENABLED).unwrap();
        let other = g
            .link(second.port(0), b.port(0), LinkFlags::ENABLED)
            .unwrap();
        assert_eq!(
            g.validate(),
            Err(GraphError::InputConflict {
                port: b.port(0),
                count: 2
            })
        );
        g.set_link(other, false).unwrap();
        g.validate().unwrap();
    }

    #[test]
    fn validation_checks_formats_names_and_immutability() {
        let (mut g, a, b) = two_nodes();
        let y10 = g.add_node(
            Node::new("y10", NodeKind::Sensor).port(Port::output("out", bus(BusCode::Y10_1X10))),
        );
        let link = g.link(y10.port(0), b.port(0), LinkFlags::DISABLED).unwrap();
        assert_eq!(g.validate(), Err(GraphError::IncompatibleFormats(link)));

        let (mut g, a2, b2) = two_nodes();
        let link = g
            .link(a2.port(0), b2.port(0), LinkFlags::IMMUTABLE)
            .unwrap();
        assert_eq!(g.set_link(link, false), Err(GraphError::Immutable(link)));
        g.set_link(link, true).unwrap();
        g.links[0].flags.enabled = false;
        assert_eq!(g.validate(), Err(GraphError::ImmutableDisabled(link)));

        let (mut g, _, _) = two_nodes();
        g.add_node(Node::new("a", NodeKind::Scaler));
        assert_eq!(g.validate(), Err(GraphError::DuplicateName("a".into())));
        let _ = (a, b);
    }

    #[test]
    fn node_builder_sets_fields() {
        let node = Node::new("pispbe", NodeKind::IspStage)
            .device("/dev/v4l-subdev3")
            .cost(CostHint::fixed(2.0, 0.0))
            .property("driver", "pispbe")
            .port(Port::input("input", Capabilities::new()))
            .port(Port::input("config", Capabilities::new()).purpose(PortPurpose::Params));
        assert_eq!(node.device.as_deref(), Some("/dev/v4l-subdev3"));
        assert_eq!(node.find_port("config").unwrap().index, 1);
        assert_eq!(
            node.find_port("config").unwrap().purpose,
            PortPurpose::Params
        );
        assert_eq!(node.properties[0].1, "pispbe");
    }
}
