//! Two cameras grouped by timestamp (`styx::multicam`), each group pushed into a Daedalus graph
//! as one synchronized tick (`styx::core::daedalus::push_group`): the graph never sees the left
//! frame of one instant with the right frame of another.
//!
//! The grouper hands out a group when both cameras have a frame within the tolerance; the push is
//! one atomic host batch (both frames plus a `FrameGroupInfo` with the spread and offsets), so
//! the tick after it has all three. Virtual cameras have no sensor clock, so this groups them by
//! when Styx took each frame (`ClockMode::Arrival`); real sensors use their sensor timestamps
//! (the default, `ClockMode::Common(Monotonic)`).
//!
//! `cargo run -p styx-examples --features daedalus --bin daedalus_multicam`

use std::time::Duration;

use daedalus::engine::{Engine, EngineConfig};
use daedalus::macros::{node, plugin};
use daedalus::runtime::NodeError;
use daedalus::runtime::plugins::PluginRegistry;
use styx::core::daedalus::{FrameGroupInfo, GroupPorts, StyxFramesPlugin, push_group};
use styx::multicam::{ClockMode, FrameGrouper, GroupConfig, GroupPolicy};
use styx::prelude::*;

const GROUPS: u32 = 10;

/// Sees both cameras' frames of one instant, and how far apart they were taken.
#[node(id = "app.stereo", inputs("left", "right", "sync"), outputs("summary"))]
fn stereo(
    left: &FrameLease,
    right: &FrameLease,
    sync: &FrameGroupInfo,
) -> Result<String, NodeError> {
    let (l, r) = (left.meta(), right.meta());
    Ok(format!(
        "group {} {}x{} + {}x{}, spread {:.2} ms, offsets {:?} ns",
        sync.sequence,
        l.format.resolution.width,
        l.format.resolution.height,
        r.format.resolution.width,
        r.format.resolution.height,
        sync.spread_ns as f64 / 1e6,
        sync.offsets_ns,
    ))
}

#[plugin(id = "app.multicam", nodes(stereo))]
struct AppNodes;

fn camera(name: &str) -> Result<CaptureHandle, CaptureError> {
    CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .name(name)
            .resolution(320, 240)
            .fps(30),
    )
    .into_device()
    .capture_request()
    .start()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = PluginRegistry::new();
    registry.install(&StyxFramesPlugin::new())?;
    let nodes = AppNodes::new();
    registry.install(&nodes)?;
    let pair = nodes.stereo.alias("stereo");
    let graph = registry
        .graph_builder()?
        .input_typed::<FrameLease>("left")?
        .input_typed::<FrameLease>("right")?
        .input_typed::<FrameGroupInfo>("sync")?
        .try_node(&pair)?
        .try_connect("left", &pair.inputs.left)?
        .try_connect("right", &pair.inputs.right)?
        .try_connect("sync", &pair.inputs.sync)?
        .try_connect(&pair.outputs.summary, "summary")?
        .build();
    let mut host = Engine::new(EngineConfig::default())?.compile_registry(&registry, graph)?;

    // Two free-running 30 fps cameras: within half a frame, both or nothing.
    let config = GroupConfig::new(16_000_000).policy(GroupPolicy::Strict);
    let mut grouper = FrameGrouper::new(config)?
        .named("stereo")
        .clock(ClockMode::Arrival);
    let left = grouper.add("left", camera("left")?)?;
    let right = grouper.add("right", camera("right")?)?;
    let ports = GroupPorts::default()
        .camera(left, "left")
        .camera(right, "right")
        .info("sync");

    // A poll loop can wait on `grouper` (AsFd) and `host.inbound_fd()` together; here the
    // grouper's blocking `recv` is enough.
    let mut ticks = 0;
    while ticks < GROUPS {
        let group = match grouper.recv(Duration::from_secs(2)) {
            RecvOutcome::Data(group) => group,
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        };
        push_group(host.host(), &ports, group)?;
        host.tick()?;
        ticks += 1;
        if let Some(summary) = host.take::<String>("summary") {
            println!("{summary}");
        }
    }
    let report = grouper.report();
    println!(
        "groups={} match_rate={:.2} spread p50={:?} ns p99={:?} ns drops={}",
        report.groups_complete,
        report.match_rate.unwrap_or(0.0),
        report.spread_p50_ns,
        report.spread_p99_ns,
        report.drops_total(),
    );
    Ok(())
}
