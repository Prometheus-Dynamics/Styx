//! Styx frames through a Daedalus graph the way a product runs one: into a host bridge as
//! [`frame_payload`] makes them, through nodes that borrow the frame and nodes that take it by
//! value, and out to the host again, many frames in a row (so recycled storage comes round);
//! and to a node taking the frame as an `Arc` that keeps it across ticks (Eidos's trackers).
#![cfg(feature = "daedalus")]

use daedalus::engine::{Engine, EngineConfig};
use daedalus::macros::{node, plugin};
use daedalus::runtime::NodeError;
use daedalus::runtime::plugins::PluginRegistry;
use styx_core::daedalus::{FRAME_TYPE_KEY, StyxFramesPlugin, frame_payload};
use styx_core::prelude::*;

/// Takes the frame by value, as a node that keeps or forwards it does.
#[node(id = "test.pass", inputs("frame"), outputs("frame"))]
fn pass(frame: FrameLease) -> Result<FrameLease, NodeError> {
    Ok(frame)
}

/// Borrows the frame.
#[node(id = "test.stamp", inputs("frame"), outputs("stamp"))]
fn stamp(frame: &FrameLease) -> Result<i64, NodeError> {
    Ok(frame.meta().timestamp as i64)
}

/// The frame before this one, kept across ticks as a tracker keeps its last frame.
static KEPT: std::sync::Mutex<Option<std::sync::Arc<FrameLease>>> = std::sync::Mutex::new(None);

/// Takes the frame as an `Arc` and keeps it until the next tick; outputs the kept frame's
/// timestamp (0 on the first).
#[node(id = "test.keep", inputs("frame"), outputs("previous"))]
fn keep(frame: std::sync::Arc<FrameLease>) -> Result<i64, NodeError> {
    let previous = KEPT.lock().unwrap().replace(frame);
    Ok(previous.map_or(0, |p| p.meta().timestamp as i64))
}

#[plugin(id = "test.frames", nodes(pass, stamp, keep))]
struct FramesTestPlugin;

fn grey(pool: &BufferPool, timestamp: u64) -> FrameLease {
    let res = Resolution::new(4, 2).unwrap();
    let mut meta = FrameMeta::new(
        MediaFormat::new(FourCc::GREY, res, ColorSpace::Srgb),
        timestamp,
    );
    meta.clock = Some(TimestampClock::Monotonic);
    FrameLease::single_plane(meta, pool.lease(), 4, 4)
}

#[test]
fn frames_go_through_a_host_bridge_graph_by_reference_and_by_value() {
    let mut registry = PluginRegistry::new();
    registry
        .install(&StyxFramesPlugin::new())
        .expect("styx frames");
    let plugin = FramesTestPlugin::new();
    registry.install(&plugin).expect("test nodes");
    let (pass, stamp) = (plugin.pass.alias("pass"), plugin.stamp.alias("stamp"));
    let keep = plugin.keep.alias("keep");
    let graph = registry
        .graph_builder()
        .expect("graph builder")
        .try_node(&pass)
        .expect("pass")
        .try_node(&stamp)
        .expect("stamp")
        .try_node(&keep)
        .expect("keep")
        .try_connect("kept", &keep.inputs.frame)
        .expect("kept -> keep")
        .try_connect(&keep.outputs.previous, "previous")
        .expect("keep -> previous")
        .try_connect("camera", &pass.inputs.frame)
        .expect("camera -> pass")
        .try_connect(&pass.outputs.frame, "processed")
        .expect("pass -> processed")
        .try_connect(&pass.outputs.frame, &stamp.inputs.frame)
        .expect("pass -> stamp")
        .try_connect(&stamp.outputs.stamp, "stamp")
        .expect("stamp -> host")
        .build();
    let engine = Engine::new(EngineConfig::cpu()).expect("engine");
    let mut graph = engine.compile_registry(&registry, graph).expect("compile");
    let pool = BufferPool::with_capacity(4, 8);
    for timestamp in 1..=8u64 {
        graph.push_payload("camera", frame_payload(grey(&pool, timestamp)));
        graph.push_payload("kept", frame_payload(grey(&pool, timestamp)));
        graph.tick().expect("tick");
        let out = graph.take_payload("processed").expect("the frame came out");
        assert_eq!(out.type_key().as_str(), FRAME_TYPE_KEY);
        let frame = out.get_ref::<FrameLease>().expect("a Styx frame");
        assert_eq!(frame.meta().timestamp, timestamp);
        assert_eq!(frame.meta().format.code, FourCc::GREY);
        let stamp = graph.take_payload("stamp").expect("the stamp came out");
        assert_eq!(stamp.get_ref::<i64>(), Some(&(timestamp as i64)));
        let previous = graph.take_payload("previous").expect("the Arc node ran");
        assert_eq!(previous.get_ref::<i64>(), Some(&(timestamp as i64 - 1)));
    }
}
