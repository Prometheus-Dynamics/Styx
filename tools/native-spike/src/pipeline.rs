//! The raw capture path in the `rp1-cfe` media graph: sensor → `csi2` → `rp1-cfe-csi2_ch0`.
//!
//! `rp1-cfe` starts the sensor only once every video node with an enabled link is streaming,
//! so the plan enables the `csi2` source → `csi2_ch0` link and disables every other mutable
//! enabled link (the front end paths libcamera or the boot default leave on).

use std::path::PathBuf;

use styx_kernel::media::{LinkFlags, PadRef, Topology};

/// The entity names this spike expects.
pub const RECEIVER: &str = "csi2";
/// The raw capture node for CSI-2 channel 0.
pub const RAW_NODE: &str = "rp1-cfe-csi2_ch0";

/// Where the pieces of the raw path are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPath {
    /// The sensor (or bridge) entity id and name.
    pub sensor: (u32, String),
    /// Sensor source pad index.
    pub sensor_pad: u32,
    /// Receiver entity id.
    pub receiver: u32,
    /// Receiver sink pad the sensor feeds.
    pub receiver_sink: u32,
    /// Receiver source pad that feeds the raw node.
    pub receiver_source: u32,
    /// Raw capture node entity id.
    pub node: u32,
    /// Its device node, when known.
    pub node_path: Option<PathBuf>,
    /// The receiver's subdev node, when known.
    pub receiver_path: Option<PathBuf>,
    /// The sensor's subdev node, when known.
    pub sensor_path: Option<PathBuf>,
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

/// Finds the raw path in a topology.
pub fn find_raw_path(topo: &Topology) -> Result<RawPath, String> {
    let receiver = topo
        .entity_by_name(RECEIVER)
        .ok_or_else(|| format!("no '{RECEIVER}' entity"))?;
    let node = topo
        .entity_by_name(RAW_NODE)
        .ok_or_else(|| format!("no '{RAW_NODE}' entity"))?;
    let into_receiver = topo.links_to(receiver.id);
    let from_sensor = into_receiver
        .iter()
        .find(|l| {
            topo.entity(l.source.entity)
                .is_some_and(|e| e.name != RECEIVER && topo.links_to(e.id).is_empty())
        })
        .ok_or_else(|| format!("nothing feeds '{RECEIVER}'"))?;
    let to_node = topo
        .links_to(node.id)
        .into_iter()
        .find(|l| l.source.entity == receiver.id)
        .ok_or_else(|| format!("no '{RECEIVER}' -> '{RAW_NODE}' link"))?;
    let sensor = topo
        .entity(from_sensor.source.entity)
        .expect("link source exists");
    Ok(RawPath {
        sensor: (sensor.id, sensor.name.clone()),
        sensor_pad: from_sensor.source.index,
        receiver: receiver.id,
        receiver_sink: from_sensor.sink.index,
        receiver_source: to_node.source.index,
        node: node.id,
        node_path: topo.devnode_path(node.id),
        receiver_path: topo.devnode_path(receiver.id),
        sensor_path: topo.devnode_path(sensor.id),
    })
}

/// The link changes that make `path` the only enabled route to memory: every other enabled,
/// mutable data link is disabled, then the receiver → raw node link is enabled if needed.
/// Disables come first (the kernel refuses two enabled links into one sink pad).
pub fn link_plan(topo: &Topology, path: &RawPath) -> Vec<LinkChange> {
    let mut plan = Vec::new();
    let mut wanted_enabled = false;
    for l in topo.data_links() {
        let wanted = l.source.entity == path.receiver
            && l.source.index == path.receiver_source
            && l.sink.entity == path.node;
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
                entity: path.receiver,
                index: path.receiver_source,
            },
            sink: PadRef {
                entity: path.node,
                index: 0,
            },
            enable: true,
        });
    }
    plan
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
pub(crate) mod tests {
    use styx_kernel::media::{Entity, EntityFlags, EntityFunction, Link, Pad, PadFlags};

    use super::*;

    /// The CM5 `rp1-cfe` graph as the device reports it, with `sensor` as the sensor entity.
    pub(crate) fn cm5_topology(sensor: &str) -> Topology {
        let ent = |id, name: &str| Entity {
            id,
            name: name.into(),
            function: EntityFunction(0),
            flags: EntityFlags(0),
        };
        let mut t = Topology {
            entities: vec![
                ent(1, "csi2"),
                ent(10, "pisp-fe"),
                ent(16, sensor),
                ent(18, "rp1-cfe-csi2_ch0"),
                ent(22, "rp1-cfe-embedded"),
                ent(34, "rp1-cfe-fe_image0"),
                ent(42, "rp1-cfe-fe_stats"),
                ent(46, "rp1-cfe-fe_config"),
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
        let csi_src4 = pad(&mut t, 1, 4, true);
        let csi_src5 = pad(&mut t, 1, 5, true);
        let fe_sink0 = pad(&mut t, 10, 0, false);
        let fe_sink1 = pad(&mut t, 10, 1, false);
        let fe_src2 = pad(&mut t, 10, 2, true);
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

    #[test]
    fn finds_the_raw_path() {
        let t = cm5_topology("ov9782 styx-sensor-bridge-cam0");
        let p = find_raw_path(&t).unwrap();
        assert_eq!(p.sensor, (16, "ov9782 styx-sensor-bridge-cam0".into()));
        assert_eq!(
            (p.sensor_pad, p.receiver_sink, p.receiver_source),
            (0, 0, 4)
        );
        assert_eq!((p.receiver, p.node), (1, 18));
    }

    #[test]
    fn plan_disables_the_front_end_and_enables_ch0_last() {
        let t = cm5_topology("ov9782 10-0060");
        let p = find_raw_path(&t).unwrap();
        let plan = link_plan(&t, &p);
        let text: Vec<String> = plan.iter().map(|c| describe(&t, c)).collect();
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
        let p = find_raw_path(&t).unwrap();
        for c in link_plan(&t, &p) {
            for l in &mut t.links {
                let (Some(src), Some(sink)) = (
                    t.pads.iter().find(|x| x.id == l.source_id),
                    t.pads.iter().find(|x| x.id == l.sink_id),
                ) else {
                    continue;
                };
                if (src.entity_id, src.index, sink.entity_id, sink.index)
                    == (c.source.entity, c.source.index, c.sink.entity, c.sink.index)
                {
                    l.flags = if c.enable {
                        LinkFlags::ENABLED
                    } else {
                        LinkFlags(0)
                    };
                }
            }
        }
        assert!(link_plan(&t, &p).is_empty());
    }

    #[test]
    fn missing_entities_are_reported() {
        let mut t = cm5_topology("s");
        t.entities.retain(|e| e.name != RAW_NODE);
        assert!(find_raw_path(&t).unwrap_err().contains(RAW_NODE));
        assert!(find_raw_path(&Topology::default()).is_err());
    }
}
