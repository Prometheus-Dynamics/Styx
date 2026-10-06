//! Camera frames in a Daedalus graph through `styx::core::daedalus` (feature `daedalus`).
//!
//! The app only writes its nodes: Styx registers the frame type, its descriptor, the metadata
//! adapter, the `daedalus:frame` provider and the inspection serializer (`StyxFramesPlugin`),
//! and wraps frames as payloads without copying them (`frame_payload`). One node takes Daedalus's
//! generic `FrameView` (`daedalus:frame` v2), as a plugin that knows nothing of Styx would: it
//! reads the format (a DRM fourcc and its kind) and the plane layout as metadata, then the same
//! camera buffer's bytes through a CPU access that ends when its guard drops (a dma-buf is
//! mapped on first use and synced only while read).
//!
//! `cargo run -p styx-examples --features daedalus --bin daedalus_frames`

use std::time::Duration;

use daedalus::engine::{Engine, EngineConfig};
use daedalus::macros::{node, plugin};
use daedalus::runtime::NodeError;
use daedalus::runtime::plugins::PluginRegistry;
use styx::core::daedalus::{
    FrameDescriptor, FrameView, StyxFramesPlugin, cpu_readable, frame_payload,
};
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

/// Daedalus's generic frame view (`daedalus:frame` v2): the planner inserts Styx's provider
/// (`daedalus.foreign:styx:framelease->daedalus:frame`). Plane metadata never touches the
/// pixels; `plane_bytes` begins a CPU access to the lease's own bytes (ended when the guard
/// drops), and a plane the CPU cannot read still has its dma-buf descriptor.
#[node(id = "app.view_summary", inputs("frame"), outputs("summary"))]
fn view_summary(frame: FrameView<'_>) -> Result<String, NodeError> {
    let format = frame.format().to_le_bytes();
    let plane = frame
        .plane(0)
        .ok_or_else(|| NodeError::InvalidInput("frame without planes".into()))?;
    let memory = match (frame.plane_bytes(0), plane.dmabuf_fd) {
        (Some(bytes), _) => format!("mapped at {:p} ({:?})", bytes.as_ptr(), plane.mapping),
        (None, Some(fd)) => format!("dma-buf fd {fd} offset {}", plane.offset),
        (None, None) => return Err(NodeError::InvalidInput("frame not readable".into())),
    };
    Ok(format!(
        "{} ({:?}) {}x{} seq {} {:?}, plane 0 stride {} {memory}",
        String::from_utf8_lossy(&format),
        frame.format_kind(),
        frame.width(),
        frame.height(),
        frame.sequence(),
        frame.residency(),
        plane.stride,
    ))
}

#[plugin(id = "app.nodes", nodes(mean_first_plane, frame_size, view_summary))]
struct AppNodes;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = PluginRegistry::new();
    registry.install(&StyxFramesPlugin::new())?;
    let nodes = AppNodes::new();
    registry.install(&nodes)?;

    let mean = nodes.mean_first_plane.alias("mean");
    let size = nodes.frame_size.alias("size");
    let view = nodes.view_summary.alias("view");
    let graph = registry
        .graph_builder()?
        .input_typed::<FrameLease>("frame")?
        .try_node(&mean)?
        .try_node(&size)?
        .try_node(&view)?
        .try_connect("frame", &mean.inputs.frame)?
        .try_connect("frame", &size.inputs.descriptor)?
        .try_connect("frame", &view.inputs.frame)?
        .try_connect(&mean.outputs.mean, "mean")?
        .try_connect(&size.outputs.size, "size")?
        .try_connect(&view.outputs.summary, "view")?
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
