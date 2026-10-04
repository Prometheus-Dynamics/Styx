# Styx frames in Daedalus graphs

`styx-core-rs` owns Styx's [Daedalus](https://github.com/Prometheus-Dynamics/Daedalus)
integration behind its optional `daedalus` feature: everything a Daedalus graph needs to know
about a `FrameLease`, registered in one place (`styx_core::daedalus`). Libraries and apps turn the
feature on and use it; none of them registers frames themselves. It follows "Integrating An
External Frame Source" in Daedalus's `docs/node-authoring.md`.

It is the only Daedalus path in Styx. The earlier `styx::graph` module and the
`daedalus-plugin` / `graph-pipeline` features were removed: build a graph with Daedalus and feed
it Styx frames as below.

## What it provides

| Item | What it is |
| --- | --- |
| `FRAME_TYPE_KEY` = `"styx:framelease"` | The type key of the frame type and of every frame payload. A public contract. |
| `FrameLease: DaedalusTypeExpr` | Node ports can take `&FrameLease` directly. |
| `FrameDescriptor` (`"styx:frame_descriptor"`) | The frame as graphs, editors and host inspection see it, never the pixels: `format` (FourCC), `width`, `height`, `color`, `timestamp_ns`, `clock`, `delta`, `crop`, `planes` (`offset`, `len`, `stride`), `residency`, `cpu_access` and `companions` (pyramid levels or a second ISP output, each with its size, planes, residency and CPU access). `FrameDescriptor::of(&frame)` builds one. |
| `styx.frame_descriptor` adapter | `MetadataOnly`: the planner inserts it on an edge from a frame port to a `&FrameDescriptor` port. It reads metadata only. |
| Value serializer | Host payload inspection (`HostGraph::inspect_payload`, `inspect_outputs`) shows a frame as its descriptor rather than an opaque summary. |
| `StyxFramesPlugin` (`"styx.frames"`) | Registers all of the above: the frame type, the descriptor types, the adapter and the serializer. Install it once per registry. |
| `frame_payload(frame)`, `shared_frame_payload(arc)` | A frame as a Daedalus `Payload` without copying it. The payload owns the frame, and with it the camera buffer, until the graph drops it. |
| `payload_residency(&frame)` | Maps the frame's memory to a Daedalus `Residency`: host-owned memory and compressed packets → `Cpu`; dma-bufs and driver or memfd buffers → `External`; GPU textures → `Gpu`. |
| `cpu_readable(&frame)` | Whether a node can read the planes in place (`CpuAccess` is not `None`). |

## Enabling it

A library or app that uses Styx through the `styx` facade:

```toml
styx = { version = "2", features = ["daedalus"] }         # passes through to styx-core-rs
daedalus = { package = "daedalus-rs", version = "2", features = ["engine", "plugins"] }
```

A library that depends only on the core types:

```toml
styx-core-rs = { version = "2", features = ["daedalus"] }
```

The module is `styx_core::daedalus`, or `styx::core::daedalus` through the facade. The feature
adds only the Daedalus types, macros and plugin registry (`daedalus-rs` with `plugins`). The app
picks the engine and any GPU features it wants.

**Daedalus source.** While the stack is stitched together, the workspace depends on the local
Daedalus checkout next to this one (`../Daedalus`, branch `dev`; see the root `Cargo.toml`). Every
workspace build needs that checkout, including CI and Docker builds, even with the feature off,
because Cargo loads path dependencies when it resolves the workspace. Switching to a git or
crates.io dependency later is a one-line change to the root `Cargo.toml`.

## Using it

```rust
use daedalus::engine::{Engine, EngineConfig};
use daedalus::macros::{node, plugin};
use daedalus::runtime::{NodeError, plugins::PluginRegistry};
use styx::core::daedalus::{FrameDescriptor, StyxFramesPlugin, frame_payload};
use styx::prelude::*;

// Pixels, in place.
#[node(id = "app.mean", inputs("frame"), outputs("mean"))]
fn mean(frame: &FrameLease) -> Result<f64, NodeError> { /* frame.planes() ... */ }

// Metadata only: reached through Styx's adapter.
#[node(id = "app.size", inputs("descriptor"), outputs("size"))]
fn size(d: &FrameDescriptor) -> Result<String, NodeError> {
    Ok(format!("{}x{}", d.width, d.height))
}

#[plugin(id = "app.nodes", nodes(mean, size))]
struct AppNodes;

let mut registry = PluginRegistry::new();
registry.install(&StyxFramesPlugin::new())?;   // once
let nodes = AppNodes::new();
registry.install(&nodes)?;
let graph = registry
    .graph_builder()?
    .input_typed::<FrameLease>("frame")        // typed: feeds both port types
    // ... nodes and edges ...
    .build();
let mut host = Engine::new(EngineConfig::default())?.compile_registry(&registry, graph)?;
host.set_latest_input("frame")?;              // live camera: replace stale frames
host.push_payload("frame", frame_payload(frame));
```

App plugins do not list `FrameLease` or `FrameDescriptor` in `types(...)` / `values(...)`:
`StyxFramesPlugin` registers them. Frames can come from any Styx source: a `CaptureHandle`, a
`MediaPipeline`, or a `FrameClient` attached to the camera service. Frames received over IPC keep
the sender's memory: their residency is `External`, and their `cpu_access` is what the sender
reported.

`examples/02_graph/daedalus_frames.rs` is the whole flow with a virtual camera:

```bash
cargo run -p styx-examples --features daedalus --bin daedalus_frames
```

## Stability

The type keys (`styx:framelease`, `styx:frame_descriptor`, `styx:plane_descriptor`,
`styx:region`, `styx:companion_descriptor`) and the plugin and adapter ids (`styx.frames`,
`styx.frame_descriptor`) are stored in graph documents, so they do not change. New descriptor
fields may be added.
