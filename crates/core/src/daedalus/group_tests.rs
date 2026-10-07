//! Frame groups as one host batch.

use ::daedalus::runtime::host_bridge::{HostBridgeHandle, HostBridgeManager};
use ::daedalus::runtime::plugins::PluginRegistry;

use super::*;
use crate::multicam::{FrameGroup, GroupConfig, GroupPolicy, Grouper};
use crate::prelude::*;

fn grey(pool: &BufferPool, timestamp: u64) -> FrameLease {
    let res = Resolution::new(8, 4).unwrap();
    let mut meta = FrameMeta::new(
        MediaFormat::new(FourCc::GREY, res, ColorSpace::Srgb),
        timestamp,
    );
    meta.clock = Some(TimestampClock::Monotonic);
    FrameLease::single_plane(meta, pool.lease(), 32, 8)
}

/// A group of cameras 0 and 2 (camera 1 missing).
fn partial_group(pool: &BufferPool) -> FrameGroup<FrameLease> {
    let config = GroupConfig::new(1_000)
        .policy(GroupPolicy::Partial)
        .deadline_ns(0);
    let mut g = Grouper::new(config);
    for _ in 0..3 {
        g.add_camera();
    }
    g.push(0, 10_000, 0, grey(pool, 10_000));
    g.push(2, 10_400, 0, grey(pool, 10_400));
    g.poll(1);
    g.pop().expect("a partial group")
}

fn bridge() -> HostBridgeHandle {
    let mut registry = PluginRegistry::new();
    registry.install(&StyxFramesPlugin::new()).unwrap();
    let manager = HostBridgeManager::new();
    manager.set_type_index(registry.type_index());
    manager.ensure_handle("host")
}

#[test]
fn info_describes_the_group() {
    let pool = BufferPool::with_capacity(4, 32);
    let group = partial_group(&pool);
    let info = FrameGroupInfo::of(&group);
    assert_eq!(info.cameras, [0, 2]);
    assert_eq!(info.offsets_ns, [0, 400]);
    assert_eq!((info.spread_ns, info.complete), (400, false));
    assert_eq!(info.reference_ns, 10_000);
}

#[test]
fn a_group_is_pushed_as_one_batch() {
    let pool = BufferPool::with_capacity(4, 32);
    let host = bridge();
    let ports = GroupPorts::new(["left", "middle", "right"]).info("sync");
    let outcomes = push_group(&host, &ports, partial_group(&pool)).expect("batch");
    assert_eq!(outcomes.len(), 3, "two frames and the info");
    assert_eq!(host.pending_inbound(), 3);
    // What the next tick takes: all three, whole.
    let mut inbound = Vec::new();
    host.take_inbound_into(&mut inbound);
    let take = |port: &str| {
        inbound
            .iter()
            .find(|p| p.port.as_str() == port)
            .map(|p| p.payload.clone())
    };
    let left = take("left").expect("left");
    assert_eq!(left.type_key().as_str(), FRAME_TYPE_KEY);
    assert_eq!(
        left.get_ref::<FrameLease>().unwrap().meta().timestamp,
        10_000
    );
    assert!(take("middle").is_none(), "camera 1 missed it");
    let right = take("right").expect("right");
    assert_eq!(
        right.get_ref::<FrameLease>().unwrap().meta().timestamp,
        10_400
    );
    let info = take("sync").expect("info");
    assert_eq!(info.type_key().as_str(), GROUP_INFO_TYPE_KEY);
    assert_eq!(info.get_ref::<FrameGroupInfo>().unwrap().cameras, [0, 2]);
    drop(inbound);
    drop((left, right, info));
    assert_eq!(pool.stats().in_use, 0, "the payloads owned the leases");
}

#[test]
fn cameras_without_a_port_are_left_out() {
    let pool = BufferPool::with_capacity(4, 32);
    let ports = GroupPorts::default().camera(2, "right");
    let payloads = group_payloads(partial_group(&pool), &ports);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].0.as_str(), "right");
    assert_eq!(pool.stats().in_use, 1, "camera 0's frame went back");
}
