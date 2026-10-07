//! Frame groups ([`crate::multicam`]) into a graph as one synchronized tick.
//!
//! Daedalus takes a host bridge's inputs for a tick under one lock, and
//! [`HostBridgeHandle::push_batch`] enqueues several payloads under that lock, waking the graph
//! once: a batch is seen whole or not at all. [`push_group`] pushes each camera's frame (as
//! [`frame_payload`](super::frame_payload), no copy) to that camera's port, plus the group's
//! [`FrameGroupInfo`] when asked, in one batch, so the graph ticks once per group with every
//! camera's frame of the same instant. Pair it with a pollable driver loop: the grouper's
//! descriptor and `HostGraph::inbound_fd()` in one `poll(2)`.
//!
//! Nodes see frames as usual (`&FrameLease`, `FrameView<'_>`, `&FrameDescriptor`), one port per
//! camera. Groups missing a camera ([`GroupPolicy::Partial`](crate::multicam::GroupPolicy))
//! leave its port empty for that tick: nodes taking every camera should take
//! `Option<&FrameLease>` (or read [`FrameGroupInfo::cameras`]), or use the `Strict` / `Latest`
//! policies, which only hand out complete groups of the cameras they need.

use ::daedalus::runtime::PortId;
use ::daedalus::runtime::host_bridge::{HostBatchOutcomes, HostBatchRejected, HostBridgeHandle};
use ::daedalus::transport::Payload;
use ::daedalus::{DaedalusToValue, DaedalusTypeExpr};
use smallvec::SmallVec;

use crate::buffer::FrameLease;
use crate::multicam::FrameGroup;

/// Type key of [`FrameGroupInfo`].
pub const GROUP_INFO_TYPE_KEY: &str = "styx:frame_group";

/// What a graph learns about a group besides its frames.
#[derive(Clone, Debug, PartialEq, DaedalusTypeExpr, DaedalusToValue)]
#[daedalus(type_key = "styx:frame_group")]
pub struct FrameGroupInfo {
    /// The grouper's group number (gaps: groups dropped as stale).
    pub sequence: u64,
    /// The anchor frame's timestamp, ns on the grouper's clock.
    pub reference_ns: u64,
    /// Latest minus earliest member timestamp, ns.
    pub spread_ns: u64,
    /// Every registered camera is in the group.
    pub complete: bool,
    /// The cameras present (grouper indices), in order.
    pub cameras: Vec<u32>,
    /// Each present camera's timestamp minus `reference_ns`, in the order of `cameras`.
    pub offsets_ns: Vec<i64>,
}

impl FrameGroupInfo {
    /// The info of `group` (its metadata, not its frames).
    pub fn of<T>(group: &FrameGroup<T>) -> Self {
        Self {
            sequence: group.sequence,
            reference_ns: group.reference_ns,
            spread_ns: group.spread_ns,
            complete: group.complete,
            cameras: group.members.iter().map(|m| m.camera as u32).collect(),
            offsets_ns: group
                .members
                .iter()
                .map(|m| crate::multicam::signed_diff(m.timestamp_ns, group.reference_ns))
                .collect(),
        }
    }
}

/// Which host input each camera of a group goes to, and where the group's info goes.
#[derive(Clone, Debug, Default)]
pub struct GroupPorts {
    cameras: SmallVec<[Option<PortId>; 4]>,
    info: Option<PortId>,
}

impl GroupPorts {
    /// Camera `i` (the grouper's index) to the `i`-th port.
    pub fn new<P: Into<PortId>>(ports: impl IntoIterator<Item = P>) -> Self {
        Self {
            cameras: ports.into_iter().map(|p| Some(p.into())).collect(),
            info: None,
        }
    }

    /// Camera `camera` to `port` (cameras without a port are dropped from the push).
    pub fn camera(mut self, camera: usize, port: impl Into<PortId>) -> Self {
        if self.cameras.len() <= camera {
            self.cameras.resize(camera + 1, None);
        }
        self.cameras[camera] = Some(port.into());
        self
    }

    /// Also push each group's [`FrameGroupInfo`] to `port`.
    pub fn info(mut self, port: impl Into<PortId>) -> Self {
        self.info = Some(port.into());
        self
    }

    /// The port of `camera`.
    pub fn port(&self, camera: usize) -> Option<&PortId> {
        self.cameras.get(camera)?.as_ref()
    }
}

/// `group` as host payloads, ready for [`HostBridgeHandle::push_batch`]: each camera's frame
/// (no copy) to its port, then the info when [`GroupPorts::info`] is set. Frames of cameras
/// without a port are dropped (their buffers go back at once).
pub fn group_payloads(
    group: FrameGroup<FrameLease>,
    ports: &GroupPorts,
) -> SmallVec<[(PortId, Payload); 5]> {
    let info = ports
        .info
        .as_ref()
        .map(|port| (port.clone(), FrameGroupInfo::of(&group)));
    let mut out: SmallVec<[(PortId, Payload); 5]> = group
        .members
        .into_iter()
        .filter_map(|m| Some((ports.port(m.camera)?.clone(), super::frame_payload(m.item))))
        .collect();
    if let Some((port, info)) = info {
        out.push((port, Payload::owned(GROUP_INFO_TYPE_KEY, info)));
    }
    out
}

/// Pushes `group` into the graph behind `host` (`HostGraph::host()`) as one batch: the next tick
/// sees all of its frames, or none. A payload failing the graph's type check rejects the whole
/// group (nothing is pushed; the frames go back).
pub fn push_group(
    host: &HostBridgeHandle,
    ports: &GroupPorts,
    group: FrameGroup<FrameLease>,
) -> Result<HostBatchOutcomes, HostBatchRejected> {
    host.push_batch(group_payloads(group, ports))
}
