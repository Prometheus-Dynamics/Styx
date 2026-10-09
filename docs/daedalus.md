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
| `StyxFramesPlugin` (`"styx.frames"`) | Registers all of the above: the frame type, the descriptor types, the adapter and the serializer, plus styx-core's build (`#[plugin(.., crate_build)]`: crate `styx_core`, its version and enabled features, exported by styx-core's `build.rs`). Install it once per registry. |
| `frame_payload(frame)`, `shared_frame_payload(arc)` | A frame as a Daedalus `Payload` without copying it. The payload owns the frame, and with it the camera buffer, until the graph drops it. |
| `payload_residency(&frame)` | Maps the frame's memory to a Daedalus `Residency`: host-owned memory and compressed packets → `Cpu`; dma-bufs and driver or memfd buffers → `External`; GPU textures → `Gpu`. |
| `cpu_readable(&frame)` | Whether a node can read the planes in place (`CpuAccess` is not `None`). |
| `FrameLease: FrameSource` | Daedalus's generic frame view, `daedalus:frame` v2: nodes take `FrameView<'_>` of a Styx frame without knowing Styx (see [Generic frame view](#generic-frame-view)). The plugin registers it as a provider (`daedalus.foreign:styx:framelease->daedalus:frame`, a `View` adapter). |
| `frame_view(&frame)`, `view_residency(&frame)`, `plane_mapping(&frame)`, `format_kind(code)` | A lease as a `FrameView` directly (borrowed, nothing allocated), and the residency, plane mapping and format kind it reports. `FrameView`, `FrameInterface`, `FrameFormatKind` and `PlaneMapping` are re-exported. |

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

**Daedalus source.** Until Daedalus 3.0 is on crates.io, Styx depends on Daedalus's `dev` branch
by its canonical git URL (`https://github.com/Prometheus-Dynamics/Daedalus.git`, `branch = "dev"`
in the root `Cargo.toml`). Apps and libraries that use Styx from git depend on Daedalus the same
way, so both resolve to one `daedalus-rs` (a `Cargo.lock` fixes the commit; `cargo update -p
daedalus-rs` moves it to the branch's head). Styx is built and tested against Daedalus `dev` at
`66659f7` (plugin ABI 9, `daedalus:frame` v2; the `Cargo.lock` is not committed, so pin it with
`cargo update -p daedalus-rs --precise 66659f7`); it needs at least `ed7ddde` (`daedalus:frame`
v2). A Daedalus with `daedalus:frame` v1 does not build with
this Styx, and v1 and v2 plugins refuse each other. To develop against a local Daedalus checkout,
override the git source in an untracked `.cargo/config.toml` (or the app's `[patch]`):

```toml
[patch."https://github.com/Prometheus-Dynamics/Daedalus.git"]
daedalus-rs = { path = "../Daedalus/crates/daedalus" }
```

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
    .input_typed::<FrameLease>("frame")?       // typed: feeds both port types
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

## Several cameras in one tick

`push_group(host.host(), &GroupPorts::new(["left", "right"]).info("sync"), group)` pushes a
`styx::multicam` frame group (every camera's frame of one instant) as one atomic host batch,
with a `FrameGroupInfo` (`styx:frame_group`: spread, offsets, cameras present), so the graph
ticks once per group and never mixes instants. See
[multi-camera-sync.md](multi-camera-sync.md#daedalus-one-tick-per-group) and
`examples/02_graph/daedalus_multicam.rs`.

## Generic frame view

Daedalus's `daedalus:frame` v2 interface (`FrameSource` / `FrameView`, Daedalus's
`docs/foreign-frame-interface.md`) is how a node reads any library's frames, including a node in
a plugin built separately from Styx. A node takes `frame: FrameView<'_>`; fed a
`styx:framelease` payload, the planner inserts Styx's provider, which retypes the payload in
place, and the node reads the lease itself through the interface's vtable. v2 separates
metadata from CPU access: reading the format and the plane layout never maps or syncs the
frame, and the bytes come from a CPU access that ends when its guard drops:

```rust
#[node(id = "app.luma_mean", inputs("frame"), outputs("mean"))]
fn luma_mean(frame: FrameView<'_>) -> Result<f64, NodeError> {
    let luma = frame.plane_bytes(0) // maps on first use, syncs; ends when `luma` drops
        .ok_or(NodeError::InvalidInput("frame is not CPU-readable".into()))?;
    Ok(luma.iter().map(|&p| f64::from(p)).sum::<f64>() / luma.len() as f64)
}
```

What the view reports:

| Field | From the lease |
| --- | --- |
| `width`, `height` | The format's size. |
| `format`, `format_kind`, `modifier` | `styx_core::format::drm::to_drm` of the FourCC, as `Pixel` (e.g. `NV12` → `NV12`/linear, `RG24` (bytes R, G, B) → `BG24`/linear) or `Bayer` (libcamera's Bayer and raw codes: `RG10`/linear, `pBAA` → `BG10` + `MIPI_FORMAT_MOD_CSI2_PACKED`, `Y10P` → `R10 ` + the same; `FrameView::is_csi2_packed` tells it from MediaTek's modifiers). Compressed formats are `Compressed` with their V4L2 fourcc (`MJPG`, `H264`, ...) and `DRM_FORMAT_MOD_INVALID`. Codes DRM has none for (`Y14 `) are `Unknown`, format `0`. |
| `timestamp_ns`, `sequence` | The frame's timestamp (in its clock, `FrameDescriptor::clock`) and the driver's sequence number (0 when unknown). |
| `residency` | `Cpu` for frames in their own host memory and compressed packets; `External` for dma-bufs and memory owned elsewhere (driver, memfd, IPC, caller buffers); `Gpu` for GPU textures. Never `Cpu` for a frame the CPU cannot read. |
| plane `len`, `stride` | The plane layout's, `u64`. |
| plane `dmabuf_fd`, `offset` | The dma-buf the plane lies in (`FrameLease::dmabuf_plane`, borrowed from the backing: duplicate it to keep it) and the plane's offset in it (`u64`), for `ExternalFrameDescriptor::from_frame_view` / `GpuContextHandle::import_dmabuf` or KMS. Without a dma-buf, no fd and `offset` is the layout's offset in its buffer. |
| plane `mapping` | From `FrameLease::cpu_access`: `Cached`, `Uncached` (uncached or write-combined memory: readable but slow, copy once if you read it more than once), or `Unmapped` (`CpuAccess::None`: `plane_bytes` gives nothing). |
| `plane_bytes(i)` | `FrameLease::begin_cpu_read` / `end_cpu_read`: the lease's own bytes in place, `None` when the CPU cannot read them (no host pointer is handed out for such memory). |

Reading metadata touches nothing, so a consumer that only forwards the planes' dma-bufs (a GPU
importer) costs no `mmap` and no cache maintenance. A CPU read is a bracketed access, counted by
the backing's `CpuReadWindow` (`styx_core::buffer`): the backing maps on its first CPU read
and keeps the mapping until it drops, starts CPU access (`DMA_BUF_IOCTL_SYNC` `SYNC_START |
SYNC_READ`) when the first open read begins and ends it when the last one ends, so consumers of
one frame reading at once share one START and one END. Styx's own reads (`FrameLease::planes`)
hold the access open until the frame drops instead; a frame read both ways syncs once. Per
backing:

| Backing | Mapping | Sync |
| --- | --- | --- |
| shared-fd (`from_dmabuf_import`, IPC imports with planes on several buffers) | each buffer mapped (`MAP_POPULATE`) on the frame's first CPU read, unmapped on drop | the mapped dma-bufs, through the window |
| IPC (camera service, frame socket: `CachedDmabuf`) | the receiver's mapping cache: each buffer mapped once, on its first CPU read, and reused for later frames in it | the dma-buf, through the window (nothing for memfds) |
| libcamera | the capture's mapping cache, on the first CPU read of a buffer | each mapped buffer, through the window |
| PiSP back end | mapped when the output buffer is first leased | the buffer (`dma_heap::sync`), through the window |
| native capture (`LeaseBuffer`) | the receiver's mapping, made with the stream's buffers | `FrameBuffer::begin_cpu` / `end_cpu` (V4L2 buffers skip the read END, which only cleans cache lines the CPU did not dirty), through the window |
| caller regions (`MemoryRegion`), imported buffers, host memory | as given | `RegionHooks::begin_cpu_read` once, or nothing |

So a dma-buf frame without CPU access (an unmapped dma-buf, `CpuAccess::None`) shows its
geometry and descriptor but no bytes, and a GPU texture or a backing that reports no dma-buf
shows neither; nodes refuse those with an error. The PiSP, libcamera, native capture, imported,
IPC and shared-fd backings report their dma-bufs.

**Lifetime.** The view borrows the payload (or the lease, for `frame_view`), which owns the
lease and with it the camera buffer, so the planes and the descriptor stay valid for as long as
the view; nothing is copied, allocated or reference counted per frame or per consumer. A payload
crossing into a stable-ABI plugin is wrapped in a handle to the same `Arc` (one reference count
increment). `crates/styx/tests/zero_alloc.rs` checks it: a camera service client reading each
frame through `FrameView` allocates once per frame (the received frame's release record) and
copies nothing; wrapping it in a payload adds `frame_payload`'s own two allocations. With a
frame socket's dma-buf frames (from the system dma-heap where there is one), a consumer that
reads only metadata and fds makes Styx map nothing, begin no CPU access and sync nothing
(`styx_core::metrics::path_counters()`: `frame_maps`, `cpu_reads`, `syncs`), and a CPU
consumer reading both planes at once makes one START and one END per frame and maps each buffer
once.

**FourCC mapping.** `styx_core::format::drm` (`to_drm`, `from_drm`, the `MAPPINGS` /
`ALIASES` / `UNMAPPED` tables, `no_std`) maps every Styx pixel format by memory layout, byte
for byte. Packed RGB differs in name between the two: DRM names a little-endian word from its
most significant end, so Styx `RG24` (= V4L2 `RGB3`) is DRM `BG24`, `BG24` is `RG24`, `RGBA`
is `AB24`, `BGRA` is `AR24`, `RG48` is `BG48`; V4L2's own `XR24` / `XB24` equal DRM's.
Greyscale wider than 8 bits (`Y10 `, `Y12 `, `Y16 `) is DRM's single-channel `R10` / `R12` /
`R16`. The kernel defines no Bayer formats: those rows follow libcamera's extension of
`drm_fourcc.h` (`DrmRegistry::Libcamera`), with the CSI-2 packing as a modifier; two of its codes
differ from V4L2's (`BA14` for 14-bit GRBG, `RGB6` for 16-bit RGGB, which is also V4L2's
48-bit RGB code). Compressed formats and 14-bit greyscale have no DRM format.

## Stability

The type keys (`styx:framelease`, `styx:frame_descriptor`, `styx:plane_descriptor`,
`styx:region`, `styx:companion_descriptor`) and the plugin and adapter ids (`styx.frames`,
`styx.frame_descriptor`, and Daedalus's `daedalus.foreign:styx:framelease->daedalus:frame` for
the frame view) are stored in graph documents, so they do not change. New descriptor fields may
be added. The frame view follows `daedalus:frame` v2 (the ids did not change from v1); a new
version of the interface is a new provider.

## Dynamic plugins built separately

A Rust-ABI dynamic plugin must come from the same cargo build as the host. When it does not,
Daedalus (plugin ABI 9) refuses it with a boundary type conflict on `styx:framelease`, and
because `StyxFramesPlugin` registers styx-core's build on both sides, the error names the
difference, e.g. ``crate `styx_core` 2.0.0: host features `daedalus,serde,std`, plugin features
`daedalus,std` (missing in plugin: serde)``. `PluginLibrary::crate_build_diff(&registry)` lists
the same differences before installing, as a warning to log.
