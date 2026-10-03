//! The raw capture route in a media graph: sensor (the bridge) → receiver → the receiver's
//! first raw video node, and the link changes that make it the only active route.
//!
//! Receivers like `rp1-cfe` start the sensor only once every video node with an enabled link is
//! streaming, so the plan disables every other enabled mutable link (the ISP front end paths the
//! boot default or libcamera leave on) before enabling the raw one.

use std::path::{Path, PathBuf};

use styx_kernel::media::{EntityFunction, LinkFlags, PadRef, Topology};

use crate::error::{NativeError, Result};

/// Where the pieces of the raw path are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRoute {
    /// Sensor (bridge) entity id.
    pub sensor: u32,
    /// Its name, e.g. `ov9782 styx-sensor-bridge-cam0`.
    pub sensor_name: String,
    /// Sensor source pad.
    pub sensor_pad: u32,
    /// Receiver entity id.
    pub receiver: u32,
    /// Receiver name, e.g. `csi2`.
    pub receiver_name: String,
    /// Receiver sink pad the sensor feeds.
    pub receiver_sink: u32,
    /// Receiver source pad that feeds the raw node.
    pub receiver_source: u32,
    /// Raw video node entity id.
    pub node: u32,
    /// Its name, e.g. `rp1-cfe-csi2_ch0`.
    pub node_name: String,
    /// The raw node's device node, when known.
    pub node_path: Option<PathBuf>,
    /// The receiver's subdev node, when known.
    pub receiver_path: Option<PathBuf>,
    /// The sensor's subdev node, when known.
    pub sensor_path: Option<PathBuf>,
    /// The embedded-data video node fed by the receiver, if there is one.
    pub embedded_node: Option<u32>,
}

/// One link change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkChange {
    /// Source pad.
    pub source: PadRef,
    /// Sink pad.
    pub sink: PadRef,
    /// Enable (true) or disable.
    pub enable: bool,
}

fn is_video(topo: &Topology, id: u32) -> bool {
    topo.entity(id)
        .is_some_and(|e| e.function == EntityFunction::IO_V4L)
}

fn is_metadata_node(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("embedded") || n.contains("meta") || n.contains("stats") || n.contains("config")
}

/// The entity whose device node is `path` (the bridge subdev).
pub fn entity_for_devnode(topo: &Topology, path: &Path) -> Option<u32> {
    topo.entities
        .iter()
        .find(|e| topo.devnode_path(e.id).as_deref() == Some(path))
        .map(|e| e.id)
}

/// Finds the raw route that starts at entity `sensor`.
pub fn find_route(topo: &Topology, sensor: u32) -> Result<RawRoute> {
    let err = |m: String| NativeError::Topology(m);
    let sensor_entity = topo
        .entity(sensor)
        .ok_or_else(|| err(format!("no entity {sensor}")))?;
    let out = topo
        .links_from(sensor)
        .into_iter()
        .min_by_key(|l| (l.source.index, l.sink.entity))
        .ok_or_else(|| err(format!("\"{}\" feeds nothing", sensor_entity.name)))?;
    let receiver = topo
        .entity(out.sink.entity)
        .ok_or_else(|| err("the sensor link ends nowhere".into()))?;
    let from_receiver = topo.links_from(receiver.id);
    let to_node = from_receiver
        .iter()
        .filter(|l| is_video(topo, l.sink.entity))
        .filter(|l| {
            topo.entity(l.sink.entity)
                .is_some_and(|e| !is_metadata_node(&e.name))
        })
        .min_by_key(|l| (l.source.index, l.sink.entity))
        .ok_or_else(|| err(format!("\"{}\" feeds no raw video node", receiver.name)))?;
    let node = topo.entity(to_node.sink.entity).expect("link sink exists");
    let embedded_node = from_receiver
        .iter()
        .filter(|l| is_video(topo, l.sink.entity))
        .find(|l| {
            topo.entity(l.sink.entity)
                .is_some_and(|e| e.name.to_ascii_lowercase().contains("embedded"))
        })
        .map(|l| l.sink.entity);
    Ok(RawRoute {
        sensor,
        sensor_name: sensor_entity.name.clone(),
        sensor_pad: out.source.index,
        receiver: receiver.id,
        receiver_name: receiver.name.clone(),
        receiver_sink: out.sink.index,
        receiver_source: to_node.source.index,
        node: node.id,
        node_name: node.name.clone(),
        node_path: topo.devnode_path(node.id),
        receiver_path: topo.devnode_path(receiver.id),
        sensor_path: topo.devnode_path(sensor),
        embedded_node,
    })
}

/// The link changes that make `route` the only enabled route to memory: every other enabled
/// mutable data link is disabled, then the receiver → raw node link is enabled if needed.
/// Disables come first (the kernel refuses two enabled links into one sink pad).
pub fn link_plan(topo: &Topology, route: &RawRoute) -> Vec<LinkChange> {
    let mut plan = Vec::new();
    let mut wanted_enabled = false;
    for l in topo.data_links() {
        let wanted = l.source.entity == route.receiver
            && l.source.index == route.receiver_source
            && l.sink.entity == route.node;
        let enabled = l.flags.contains(LinkFlags::ENABLED);
        if wanted {
            wanted_enabled = enabled;
            continue;
        }
        if enabled && !l.flags.contains(LinkFlags::IMMUTABLE) {
            plan.push(LinkChange {
                source: l.source,
                sink: l.sink,
                enable: false,
            });
        }
    }
    if !wanted_enabled {
        plan.push(LinkChange {
            source: PadRef {
                entity: route.receiver,
                index: route.receiver_source,
            },
            sink: PadRef {
                entity: route.node,
                index: 0,
            },
            enable: true,
        });
    }
    plan
}

/// Applies link changes to a topology description (what the kernel does on
/// `MEDIA_IOC_SETUP_LINK`), so the device graph can follow without re-reading the topology.
pub fn apply_plan(topo: &mut Topology, plan: &[LinkChange]) {
    for c in plan {
        let pads: Vec<(u32, u32, u32)> = topo
            .pads
            .iter()
            .map(|p| (p.id, p.entity_id, p.index))
            .collect();
        let find = |r: PadRef| {
            pads.iter()
                .find(|(_, e, i)| *e == r.entity && *i == r.index)
                .map(|(id, _, _)| *id)
        };
        let (Some(src), Some(sink)) = (find(c.source), find(c.sink)) else {
            continue;
        };
        for l in &mut topo.links {
            if l.source_id == src && l.sink_id == sink {
                let others = LinkFlags(l.flags.0 & !LinkFlags::ENABLED.0);
                l.flags = if c.enable {
                    others | LinkFlags::ENABLED
                } else {
                    others
                };
            }
        }
    }
}

/// Human-readable description of a link change.
pub fn describe(topo: &Topology, c: &LinkChange) -> String {
    let name = |id: u32| {
        topo.entity(id)
            .map_or_else(|| format!("#{id}"), |e| e.name.clone())
    };
    format!(
        "{} \"{}\":{} -> \"{}\":{}",
        if c.enable { "enable " } else { "disable" },
        name(c.source.entity),
        c.source.index,
        name(c.sink.entity),
        c.sink.index
    )
}

#[cfg(test)]
pub(crate) mod testing {
    use styx_kernel::media::Topology;
    use styx_kernel::media::{Entity, EntityFlags, EntityFunction, Link, LinkFlags, Pad, PadFlags};

    /// The CM5 `rp1-cfe` graph as the device reports it, with `sensor` as the bridge entity
    /// (entity 16).
    pub(crate) fn cm5_topology(sensor: &str) -> Topology {
        let ent = |id, name: &str, f: EntityFunction| Entity {
            id,
            name: name.into(),
            function: f,
            flags: EntityFlags(0),
        };
        let io = EntityFunction::IO_V4L;
        let mut t = Topology {
            entities: vec![
                ent(1, "csi2", EntityFunction::VID_IF_BRIDGE),
                ent(10, "pisp-fe", EntityFunction::PROC_VIDEO_SCALER),
                ent(16, sensor, EntityFunction::CAM_SENSOR),
                ent(18, "rp1-cfe-csi2_ch0", io),
                ent(22, "rp1-cfe-embedded", io),
                ent(34, "rp1-cfe-fe_image0", io),
                ent(42, "rp1-cfe-fe_stats", io),
                ent(46, "rp1-cfe-fe_config", io),
            ],
            ..Default::default()
        };
        let mut pad_id = 100;
        let mut pad = |t: &mut Topology, entity, index, source: bool| {
            pad_id += 1;
            t.pads.push(Pad {
                id: pad_id,
                entity_id: entity,
                flags: if source {
                    PadFlags::SOURCE
                } else {
                    PadFlags::SINK
                },
                index,
            });
            pad_id
        };
        let mut link_id = 500;
        let mut link = |t: &mut Topology, src, sink, flags: LinkFlags| {
            link_id += 1;
            t.links.push(Link {
                id: link_id,
                source_id: src,
                sink_id: sink,
                flags,
            });
        };
        let csi_sink0 = pad(&mut t, 1, 0, false);
        for i in 1..4 {
            pad(&mut t, 1, i, false);
        }
        let csi_src4 = pad(&mut t, 1, 4, true);
        let csi_src5 = pad(&mut t, 1, 5, true);
        let fe_sink0 = pad(&mut t, 10, 0, false);
        let fe_sink1 = pad(&mut t, 10, 1, false);
        let fe_src2 = pad(&mut t, 10, 2, true);
        pad(&mut t, 10, 3, true);
        let fe_src4 = pad(&mut t, 10, 4, true);
        let sensor_src = pad(&mut t, 16, 0, true);
        let ch0 = pad(&mut t, 18, 0, false);
        let emb = pad(&mut t, 22, 0, false);
        let img0 = pad(&mut t, 34, 0, false);
        let stats = pad(&mut t, 42, 0, false);
        let config = pad(&mut t, 46, 0, true);
        let on = LinkFlags::ENABLED;
        link(&mut t, sensor_src, csi_sink0, on | LinkFlags::IMMUTABLE);
        link(&mut t, csi_src4, ch0, LinkFlags(0));
        link(&mut t, csi_src4, fe_sink0, on);
        link(&mut t, csi_src5, emb, LinkFlags(0));
        link(&mut t, fe_src2, img0, on);
        link(&mut t, fe_src4, stats, on);
        link(&mut t, config, fe_sink1, on);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::testing::cm5_topology;
    use super::*;

    #[test]
    fn finds_the_raw_route_from_the_bridge() {
        let t = cm5_topology("ov9782 styx-sensor-bridge-cam0");
        let r = find_route(&t, 16).unwrap();
        assert_eq!(r.sensor_name, "ov9782 styx-sensor-bridge-cam0");
        assert_eq!(
            (r.sensor_pad, r.receiver_sink, r.receiver_source),
            (0, 0, 4)
        );
        assert_eq!((r.receiver, r.node), (1, 18));
        assert_eq!(r.receiver_name, "csi2");
        assert_eq!(r.node_name, "rp1-cfe-csi2_ch0");
        assert_eq!(r.embedded_node, Some(22));
    }

    #[test]
    fn plan_disables_the_front_end_and_enables_ch0_last() {
        let t = cm5_topology("ov9782 10-0060");
        let r = find_route(&t, 16).unwrap();
        let text: Vec<String> = link_plan(&t, &r).iter().map(|c| describe(&t, c)).collect();
        assert_eq!(
            text,
            [
                "disable \"csi2\":4 -> \"pisp-fe\":0",
                "disable \"pisp-fe\":2 -> \"rp1-cfe-fe_image0\":0",
                "disable \"pisp-fe\":4 -> \"rp1-cfe-fe_stats\":0",
                "disable \"rp1-cfe-fe_config\":0 -> \"pisp-fe\":1",
                "enable  \"csi2\":4 -> \"rp1-cfe-csi2_ch0\":0",
            ]
        );
    }

    #[test]
    fn plan_is_empty_once_applied() {
        let mut t = cm5_topology("s");
        let r = find_route(&t, 16).unwrap();
        let plan = link_plan(&t, &r);
        apply_plan(&mut t, &plan);
        assert!(link_plan(&t, &r).is_empty());
        // The immutable sensor link keeps its flags.
        assert!(
            t.links[0]
                .flags
                .contains(LinkFlags::ENABLED | LinkFlags::IMMUTABLE)
        );
    }

    #[test]
    fn missing_pieces_are_reported() {
        let mut t = cm5_topology("s");
        t.entities.retain(|e| e.name != "rp1-cfe-csi2_ch0");
        let e = find_route(&t, 16).unwrap_err().to_string();
        assert!(e.contains("no raw video node"), "{e}");
        assert!(find_route(&Topology::default(), 16).is_err());
        let t = cm5_topology("s");
        assert!(find_route(&t, 18).is_err());
    }
}
