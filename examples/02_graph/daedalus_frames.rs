//! Camera frames in a Daedalus graph through `styx::core::daedalus` (feature `daedalus`).
//!
//! The app only writes its nodes: Styx registers the frame type, its descriptor, the metadata
//! adapter and the inspection serializer (`StyxFramesPlugin`), and wraps frames as payloads
//! without copying them (`frame_payload`).
//!
//! `cargo run -p styx-examples --features daedalus --bin daedalus_frames`

use std::time::Duration;

use daedalus::engine::{Engine, EngineConfig};
use daedalus::macros::{node, plugin};
use daedalus::runtime::NodeError;
use daedalus::runtime::plugins::PluginRegistry;
use styx::core::daedalus::{FrameDescriptor, StyxFramesPlugin, cpu_readable, frame_payload};
use styx::prelude::*;

const FRAMES: u32 = 8;

/// Reads the pixels in place: the node gets the frame the camera filled, not a copy.
#[node(id = "app.mean_first_plane", inputs("frame"), outputs("mean"))]
fn mean_first_plane(frame: &FrameLease) -> Result<f64, NodeError> {
    if !cpu_readable(frame) {
        return Err(NodeError::InvalidInput("frame not CPU-readable".into()));
    }
    let planes = frame.planes();
    let plane = planes
        .first()
        .ok_or_else(|| NodeError::InvalidInput("frame without planes".into()))?;
    let data = plane.data();
    let sum: u64 = data.iter().map(|&b| u64::from(b)).sum();
    Ok(sum as f64 / data.len().max(1) as f64)
}

/// Needs only the metadata: the planner inserts Styx's `styx.frame_descriptor` adapter.
#[node(id = "app.frame_size", inputs("descriptor"), outputs("size"))]
fn frame_size(descriptor: &FrameDescriptor) -> Result<String, NodeError> {
    Ok(format!(
        "{} {}x{}",
        descriptor.format, descriptor.width, descriptor.height
    ))
}

#[plugin(id = "app.nodes", nodes(mean_first_plane, frame_size))]
struct AppNodes;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = PluginRegistry::new();
    registry.install(&StyxFramesPlugin::new())?;
    let nodes = AppNodes::new();
    registry.install(&nodes)?;

    let mean = nodes.mean_first_plane.alias("mean");
    let size = nodes.frame_size.alias("size");
    let graph = registry
        .graph_builder()?
        .input_typed::<FrameLease>("frame")
        .try_node(&mean)?
        .try_node(&size)?
        .try_connect("frame", &mean.inputs.frame)?
        .try_connect("frame", &size.inputs.descriptor)?
        .try_connect(&mean.outputs.mean, "mean")?
        .try_connect(&size.outputs.size, "size")?
        .build();
    let mut host = Engine::new(EngineConfig::default())?.compile_registry(&registry, graph)?;
    for edge in host.explain_plan().edges {
        if !edge.adapter_steps.is_empty() {
            println!(
                "edge {} -> {} adapters: {:?}",
                edge.from_port, edge.to_port, edge.adapter_steps
            );
        }
    }

    // The virtual camera sends black frames (mean 0); any backend works the same way.
    let capture = CaptureRequest::virtual_source(
        VirtualSourceConfig::new()
            .name("virtual-daedalus")
            .resolution(320, 240)
            .fps(60),
    )
    .into_device()
    .capture_request()
    .start()?;
    let mut frames = 0;
    while frames < FRAMES {
        let frame = match capture.recv_blocking(Duration::from_secs(1)) {
            RecvOutcome::Data(frame) => frame,
            RecvOutcome::Empty => continue,
            RecvOutcome::Closed => break,
        };
        let payload = frame_payload(frame);
        if frames == 0 {
            println!("input frame: {}", host.inspect_payload(&payload).to_json());
        }
        host.push_payload("frame", payload);
        host.tick()?;
        frames += 1;
        let outputs: Vec<String> = host
            .inspect_outputs()
            .into_iter()
            .map(|(port, inspection)| format!("{port}={}", inspection.to_json()))
            .collect();
        println!("frame {frames}: {}", outputs.join(" "));
    }
    capture.stop();
    println!("frames={frames}");
    Ok(())
}
