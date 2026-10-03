//! Paths through a device graph: from a sensor to a sink where frames land in memory.

use std::collections::HashMap;

use crate::caps::{Cost, Size};
use crate::graph::{DeviceGraph, GraphError, LinkId, NodeId, NodeKind, PortPurpose, PortRef};

/// A route from a sensor to a sink: the links it crosses, in order.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct GraphPath {
    links: Vec<LinkId>,
    nodes: Vec<NodeId>,
    /// The sink's input port.
    output: PortRef,
    purpose: PortPurpose,
}

impl GraphPath {
    /// The sensor the path starts at.
    pub fn sensor(&self) -> NodeId {
        self.nodes[0]
    }

    /// The sink the path ends at.
    pub fn sink(&self) -> NodeId {
        self.output.node
    }

    /// The sink's input port.
    pub fn output(&self) -> PortRef {
        self.output
    }

    /// What arrives at the sink (pixels, statistics, metadata).
    pub fn purpose(&self) -> PortPurpose {
        self.purpose
    }

    /// Nodes in order, sensor first.
    pub fn nodes(&self) -> &[NodeId] {
        &self.nodes
    }

    pub fn links(&self) -> &[LinkId] {
        &self.links
    }

    pub fn uses(&self, node: NodeId) -> bool {
        self.nodes.contains(&node)
    }

    /// Estimated cost of one `size` frame along the path.
    pub fn cost(&self, graph: &DeviceGraph, size: Size) -> Cost {
        self.nodes
            .iter()
            .filter_map(|id| graph.node(*id).ok())
            .fold(Cost::ZERO, |sum, node| sum + node.cost.estimate(size))
    }

    /// `ov9782 -> csi2 -> pisp-fe -> fe_image0 -> pispbe -> be_output0`.
    pub fn describe(&self, graph: &DeviceGraph) -> String {
        self.nodes
            .iter()
            .map(|id| graph.node(*id).map_or("?", |n| n.name.as_str()))
            .collect::<Vec<_>>()
            .join(" -> ")
    }
}

/// Link changes that make a set of paths active.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LinkPlan {
    pub enable: Vec<LinkId>,
    /// Enabled links into the paths' input ports that would compete with them.
    pub disable: Vec<LinkId>,
}

impl LinkPlan {
    pub fn is_empty(&self) -> bool {
        self.enable.is_empty() && self.disable.is_empty()
    }
}

impl DeviceGraph {
    /// Every path from `sensor` to a sink. A path continues past a sink when the sink feeds a
    /// memory-reading stage (e.g. PiSP front end output feeding the back end), so both the
    /// intermediate and the final sinks are listed. Disabled mutable links are followed (they
    /// can be enabled); nodes are assumed to route any input to any output.
    pub fn paths_from(&self, sensor: NodeId) -> Vec<GraphPath> {
        let mut found = Vec::new();
        if self.node(sensor).is_ok() {
            let mut nodes = vec![sensor];
            let mut links = Vec::new();
            self.walk(sensor, &mut nodes, &mut links, &mut found);
        }
        found
    }

    /// Paths from `sensor` that deliver pixels.
    pub fn output_paths(&self, sensor: NodeId) -> Vec<GraphPath> {
        self.paths_from(sensor)
            .into_iter()
            .filter(|p| p.purpose == PortPurpose::Pixels)
            .collect()
    }

    /// Paths from every sensor.
    pub fn all_paths(&self) -> Vec<GraphPath> {
        self.sensors().flat_map(|s| self.paths_from(s.id)).collect()
    }

    fn walk(
        &self,
        at: NodeId,
        nodes: &mut Vec<NodeId>,
        links: &mut Vec<LinkId>,
        found: &mut Vec<GraphPath>,
    ) {
        let Ok(node) = self.node(at) else { return };
        for port in node.outputs() {
            let from = at.port(port.index);
            for link in self.links_from(from) {
                if nodes.contains(&link.to.node) {
                    continue;
                }
                let Ok(input) = self.port(link.to) else {
                    continue;
                };
                nodes.push(link.to.node);
                links.push(link.id);
                let target = self.node(link.to.node).map(|n| &n.kind);
                if target == Ok(&NodeKind::Sink) {
                    found.push(GraphPath {
                        links: links.clone(),
                        nodes: nodes.clone(),
                        output: link.to,
                        purpose: purpose_of(port.purpose, input.purpose),
                    });
                }
                self.walk(link.to.node, nodes, links, found);
                nodes.pop();
                links.pop();
            }
        }
    }

    /// Check that `path` still matches this graph: its links exist, connect in order and none
    /// is immutable and disabled.
    pub fn check_path(&self, path: &GraphPath) -> Result<(), GraphError> {
        let broken = |why: &str| GraphError::BrokenPath(why.to_string());
        if path.nodes.len() != path.links.len() + 1 {
            return Err(broken("node and link counts disagree"));
        }
        if self.node(path.sensor())?.kind != NodeKind::Sensor {
            return Err(broken("does not start at a sensor"));
        }
        for (i, id) in path.links.iter().enumerate() {
            let link = self.link_by_id(*id)?;
            if link.from.node != path.nodes[i] || link.to.node != path.nodes[i + 1] {
                return Err(broken("links are not connected in order"));
            }
            if link.flags.immutable && !link.flags.enabled {
                return Err(GraphError::ImmutableDisabled(*id));
            }
        }
        let last = self.link_by_id(*path.links.last().ok_or_else(|| broken("empty"))?)?;
        if last.to != path.output || self.node(path.sink())?.kind != NodeKind::Sink {
            return Err(broken("does not end at a sink"));
        }
        Ok(())
    }

    /// The link changes that make all of `paths` active together: enable their links, disable
    /// other enabled links into the same input ports. Fails when two paths need different links
    /// into one input port, or an immutable link is in the way.
    pub fn link_plan(&self, paths: &[&GraphPath]) -> Result<LinkPlan, GraphError> {
        let mut chosen: HashMap<PortRef, LinkId> = HashMap::new();
        let mut plan = LinkPlan::default();
        for path in paths {
            self.check_path(path)?;
            for id in &path.links {
                let link = self.link_by_id(*id)?;
                match chosen.insert(link.to, *id) {
                    Some(previous) if previous != *id => {
                        return Err(GraphError::InputConflict {
                            port: link.to,
                            count: 2,
                        });
                    }
                    Some(_) => continue,
                    None => {}
                }
                if !link.flags.enabled {
                    plan.enable.push(*id);
                }
            }
        }
        for (port, keep) in &chosen {
            for other in self.links_to(*port) {
                if other.id == *keep || !other.flags.enabled {
                    continue;
                }
                if other.flags.immutable {
                    return Err(GraphError::Immutable(other.id));
                }
                plan.disable.push(other.id);
            }
        }
        plan.enable.sort();
        plan.disable.sort();
        Ok(plan)
    }

    /// Apply a link plan to this description (providers apply it to hardware too).
    pub fn apply(&mut self, plan: &LinkPlan) -> Result<(), GraphError> {
        for id in &plan.disable {
            self.set_link(*id, false)?;
        }
        for id in &plan.enable {
            self.set_link(*id, true)?;
        }
        Ok(())
    }
}

/// A path carries statistics or metadata if either end of its last link says so.
fn purpose_of(out: PortPurpose, input: PortPurpose) -> PortPurpose {
    if input == PortPurpose::Pixels {
        out
    } else {
        input
    }
}

#[cfg(test)]
mod tests {
    use crate::caps::{Cost, Size};
    use crate::graph::{GraphError, LinkId, PortPurpose};
    use crate::sample;

    #[test]
    fn cm5_paths_reach_both_back_end_outputs_through_memory() {
        let g = sample::cm5_ov9782();
        g.validate().unwrap();
        let sensor = g.find("ov9782").unwrap().id;
        let pixels = g.output_paths(sensor);
        let names: Vec<String> = pixels.iter().map(|p| p.describe(&g)).collect();
        assert!(names.contains(&"ov9782 -> csi2 -> csi2_ch0".to_string()));
        assert!(names.contains(&"ov9782 -> csi2 -> pisp-fe -> fe_image0".to_string()));
        for out in ["be_output0", "be_output1"] {
            let full = format!("ov9782 -> csi2 -> pisp-fe -> fe_image0 -> pispbe -> {out}");
            assert!(names.contains(&full), "{names:?}");
        }
        let stats: Vec<_> = g
            .paths_from(sensor)
            .into_iter()
            .filter(|p| p.purpose() == PortPurpose::Stats)
            .collect();
        assert_eq!(stats.len(), 1);
        assert_eq!(g.node(stats[0].sink()).unwrap().name, "fe_stats");

        let be = g.find("pispbe").unwrap().id;
        let through_be: Vec<_> = pixels.iter().filter(|p| p.uses(be)).collect();
        assert_eq!(through_be.len(), 2);
        assert_eq!(through_be[0].sensor(), sensor);
        assert!(through_be[0].cost(&g, Size::new(1280, 800)).latency_ms > 0.0);
        assert_eq!(g.all_paths().len(), g.paths_from(sensor).len());
    }

    #[test]
    fn link_plan_enables_and_resolves_conflicts() {
        let mut g = sample::cm5_ov9782();
        let sensor = g.find("ov9782").unwrap().id;
        let be = g.find("pispbe").unwrap().id;
        let paths: Vec<_> = g
            .output_paths(sensor)
            .into_iter()
            .filter(|p| p.uses(be))
            .collect();
        let plan = g.link_plan(&paths.iter().collect::<Vec<_>>()).unwrap();
        assert!(!plan.enable.is_empty());
        g.apply(&plan).unwrap();
        g.validate().unwrap();
        assert!(g.link_plan(&[&paths[0]]).unwrap().is_empty());

        // Feed the back end from the raw CSI-2 node instead: the fe_image0 link must go.
        let raw = g.find_port("csi2_ch0", "out").unwrap();
        let be_in = g.find_port("pispbe", "input").unwrap();
        let alt = g
            .memory_link(raw, be_in, crate::LinkFlags::DISABLED)
            .unwrap();
        let alt_path = g
            .output_paths(sensor)
            .into_iter()
            .find(|p| p.links().contains(&alt))
            .unwrap();
        let plan = g.link_plan(&[&alt_path]).unwrap();
        assert_eq!(plan.enable, vec![alt]);
        assert_eq!(plan.disable.len(), 1);
        assert!(matches!(
            g.link_plan(&[&alt_path, &paths[0]]),
            Err(GraphError::InputConflict { .. })
        ));
    }

    #[test]
    fn broken_paths_are_rejected() {
        let g = sample::cm5_ov9782();
        let uvc = sample::uvc_camera();
        let sensor = uvc.sensors().next().unwrap().id;
        let path = uvc.output_paths(sensor).remove(0);
        assert!(uvc.check_path(&path).is_ok());
        // A path from another graph does not line up with this one.
        let cm5_sensor = g.find("ov9782").unwrap().id;
        for cm5_path in g
            .paths_from(cm5_sensor)
            .iter()
            .filter(|p| p.links().len() > 2)
        {
            assert!(uvc.check_path(cm5_path).is_err());
        }
        let mut bad = path.clone();
        bad.links.push(LinkId(99));
        assert!(uvc.check_path(&bad).is_err());
    }

    #[test]
    fn uvc_has_one_pixel_path() {
        let g = sample::uvc_camera();
        g.validate().unwrap();
        let sensor = g.sensors().next().unwrap().id;
        let paths = g.output_paths(sensor);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].describe(&g), "uvc-sensor -> uvc -> video0");
        assert!(paths[0].cost(&g, Size::new(1280, 720)).latency_ms >= 8.0);
        assert!(g.paths_from(crate::NodeId(42)).is_empty());
        assert_eq!(paths[0].cost(&g, Size::default()).cpu_ms, Cost::ZERO.cpu_ms);
    }
}
