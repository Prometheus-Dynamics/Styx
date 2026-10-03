# Device graph, providers and the async core (`styx-graph`)

`styx-graph` describes camera hardware as a graph, defines how providers discover and drive it,
and supplies a runtime-agnostic async core for pollable devices. It depends on `libc`,
`futures-core` and `thiserror` only; nothing from the rest of Styx.

## The model

```
DeviceGraph
  Node      id, name, kind, device path, ports, cost hint, properties
  Port      index (= V4L2 pad index), name, direction, purpose, capabilities
  Link      from (output port) -> to (input port), flags {enabled, immutable}, medium {Bus, Memory}
```

**Node kinds**: `Sensor`, `Receiver(Csi2 | Parallel | Usb | Network | Other)`, `IspStage`,
`Scaler`, `Codec`, `Compute(Gpu | Npu | Cpu)`, `Sink`, `Other(String)`.

A `Sink` is a buffer queue in memory (a V4L2 video node): frames land there and can be handed to
the application, or fed on through a `Memory` link to a stage that reads memory (the PiSP back
end, a decoder). Bus links are media controller links; memory links are "userspace passes these
buffers along" and are what makes multi-pass pipelines (front end → memory → back end) and
software stages (capture → decoder) expressible in one graph.

**Port purpose**: `Pixels`, `Stats`, `Params`, `Metadata`. Statistics and parameter buffers are
ports like any other, so an algorithm layer can find "the statistics sink of this sensor's
path" the same way the planner finds pixel outputs.

**Capabilities** (per port):

- formats: `FormatCode::Memory(FourCc)` (V4L2 pixel formats) or `FormatCode::Bus(BusCode)`
  (`MEDIA_BUS_FMT_*`), each with size ranges (`Discrete` / `Stepwise` with step alignment) and
  frame interval ranges (`Discrete` / `Stepwise`; `Fraction` compares by value, 2/60 == 1/30).
  Empty lists mean "not constrained here".
- memory domains: a set of `CPU`, `DMABUF`, `GPU`, `NPU`.

**Cost hints** (per node): `fixed + per_megapixel × MP`, each a `Cost { latency_ms, cpu_ms }`,
the same model as the planner's `StepCost` constants in `crates/styx/src/planner/cost.rs`.

### Queries

- `paths_from(sensor)`: every `GraphPath` from a sensor to a sink. Walks output ports through
  enabled and *mutable disabled* links (they can be enabled) and continues past sinks with
  outgoing memory links, so both `fe_image0` and `be_output0` are listed for the CM5.
  `output_paths` keeps pixel paths; `all_paths` covers every sensor.
- `GraphPath`: `sensor()`, `sink()`, `output()` (the sink port), `nodes()`, `links()`,
  `uses(node)`, `purpose()`, `cost(graph, size)`, `describe(graph)`.
- `validate()`: unique node names; links go output → input between existing ports; immutable
  links are enabled; at most one enabled link into an input port; linked ports share a format
  when both declare formats in the same namespace (bus ↔ memory boundaries are conversions and
  not compared).
- `check_path(path)`: the path still lines up with this graph (after hotplug or reconfiguration).
- `link_plan(&[paths])` → `LinkPlan { enable, disable }`: activate several paths together (two
  ISP outputs sharing a prefix), disabling competing enabled links into the same input ports;
  errors on two paths needing different links into one port, or an immutable link in the way.
  `apply(plan)` updates the description; providers also apply it to hardware
  (`MEDIA_IOC_SETUP_LINK`).

## Example: Raspberry Pi CM5, OV9782 on CSI-2 (`sample::cm5_ov9782`)

```
ov9782 (Sensor, /dev/v4l-subdev2, SGRBG10_1X10 1280x800 ... 1-120 fps)
  |  bus, immutable
csi2 (Receiver Csi2, /dev/v4l-subdev0)
  |-- ch0 --> csi2_ch0 (Sink, /dev/video0: raw BA10 / PC1R in memory)
  '-- fe ---> pisp-fe (IspStage, /dev/v4l-subdev1; config: Params input)
                |-- stats --> fe_stats (Sink, /dev/video6, purpose Stats, RPFS)
                '-- image0 -> fe_image0 (Sink, /dev/video4)
                                 : memory link (disabled until configured)
                                 v
                              pispbe (IspStage; input, config: Params input)
                                |-- output0 --> be_output0 (Sink: NV12/YU12/RGB3/GREY, 64..4096)
                                '-- output1 --> be_output1 (Sink: same)
```

Pixel paths: `ov9782 → csi2 → csi2_ch0` (raw), `… → pisp-fe → fe_image0` (front-end processed
raw), and `… → fe_image0 → pispbe → be_output0|1`. The stats path is
`… → pisp-fe → fe_stats`. Configuring both back end outputs yields one `LinkPlan` that enables
the `fe_image0 → pispbe` memory link. Feeding the back end from `csi2_ch0` instead is another
memory link into `pispbe:input`, and `link_plan` disables the `fe_image0` one.

## Example: a UVC camera (`sample::uvc_camera`)

```
uvc-sensor (Sensor, FIXED) --immutable--> uvc (Receiver Usb, 046d:0825, cost ≈ 0.8 frame)
    --immutable--> video0 (Sink, /dev/video0: MJPG 1280x720|640x480 @30|15, YUYV 640x480 @30)
```

With a decoder the planner would add (at planning time, not discovery):
`video0 --memory--> mjpeg-decode (Codec or Compute(Cpu), cost per MP) --> decoded (Sink)`.

## Providers

```rust
pub trait Provider: Send + Sync {
    type Frame: Send + 'static;
    fn name(&self) -> &str;                                   // "v4l2", "libcamera", "mock"
    fn discover(&self) -> Result<Vec<DeviceInfo>, ProviderError>;
    fn hotplug(&self) -> HotplugStream;                       // Added(DeviceInfo) / Removed(key)
    fn open(&self, key: &DeviceKey) -> Result<Box<dyn Device<Frame = Self::Frame>>, ProviderError>;
}

pub trait Device: Send {
    type Frame: Send + 'static;
    fn info(&self) -> &DeviceInfo;
    fn graph(&self) -> &DeviceGraph;                          // link states follow configuration
    fn configure(&mut self, streams: &[StreamConfig]) -> Result<(), ProviderError>;
    fn start(&mut self) -> Result<Vec<OutputStream<Self::Frame>>, ProviderError>;
    fn stop(&mut self) -> Result<(), ProviderError>;
}
```

- `DeviceInfo { key, name, identity, graph, properties }`. `key` is stable within the provider;
  `identity` holds fingerprints (`usb:VID:PID:serial`, device-tree paths) so one physical camera
  seen by two providers merges, exactly like `DeviceIdentity.keys` and `merge_backend` today.
- `StreamConfig { path, format, size, interval }`; `StreamConfig::check(graph)` verifies the
  path, the sink's format/size, and the interval against every port on the path that lists
  intervals.
- `configure` replaces the whole configuration (all streams of one capture at once), computes
  and applies the `LinkPlan`, and fails while streaming (`Busy`).
- `start` returns one `OutputStream { output, config, frames }` per stream, in configuration
  order. `frames` is a `futures_core::Stream<Item = Result<Frame, ProviderError>>`; it ends after
  `stop`, and yields `Err(Disconnected)` then ends on unplug.
- `open` is exclusive per device (`Busy` otherwise); dropping the device stops it.
- The frame type is opaque. The Styx integration picks one type for all its providers
  (`styx_core` `FrameLease`), so a registry of providers is `Vec<Box<dyn Provider<Frame =
  FrameLease>>>`.

`MockProvider` implements this with the graph `mock-sensor → mock-isp → {main, preview, stats}`,
patterned frames (`byte i = i + sequence`), per-stream rates from the configured interval (30 fps
by default), frames of one exposure sharing a sequence number across outputs, `plug`/`unplug`
with hotplug events, and busy/disconnect behaviour matching the contract.

## The async core (`styx_graph::rt`)

- **Reactor**: one thread per reactor waiting in `epoll_wait` (no timeout). It only wakes
  `std::task::Waker`s; it never polls futures, so it works with any executor.
  `Reactor::global()` starts on first use; `Reactor::new()` gives a private one whose thread
  exits when the last handle goes (shutdown through an eventfd).
- **Readiness**: descriptors are registered **one-shot, level-triggered**. Each wait adds a
  waiter (interest + waker) and re-arms epoll with the union of pending interests; an event
  wakes the matching waiters (errors and hang-ups wake everyone) and re-arms for the rest.
  Nothing is cached between waits, so a wait always reflects the descriptor's current state and
  there is no "clear readiness" step to forget. The cost is one `epoll_ctl` per wait, negligible
  at frame rates.
- `AsyncFd<T: AsFd>`: `readable()`, `writable()`, `priority()`, `ready(interest)`, and
  `io_with`/`read_with`/`write_with` that retry an operation on `WouldBlock` (for V4L2: `DQBUF`
  after `readable`, `DQEVENT` after `priority`). Dropping or `into_inner` leaves epoll before
  the descriptor closes.
- Free functions `readable(fd)`, `writable(fd)`, `priority(fd)` on a `BorrowedFd` register a
  `dup` of it for one wait (works even if the fd is registered elsewhere).
- **Timers**: one `timerfd` in the reactor, armed for the earliest deadline of a `BTreeMap`
  (ns precision). `sleep`, `sleep_until`, `Sleep::reset` (periodic ticks without drift),
  `timeout(duration, future)` → `Result<T, Elapsed>`.
- `block_on(future)` parks the current thread; `next(&mut stream)` gives streams a `.next()`
  without extra crates.
- Unsafe code is confined to `rt/sys.rs` (the syscalls); the crate is `#![deny(unsafe_code)]`
  elsewhere.

Tested with pipes, eventfds and TCP out-of-band data (for `EPOLLPRI`): waking from another
thread, retries on `WouldBlock`, writability after draining, hang-ups, several waiters on one
fd, dropped waiters, deregistration, private reactors, timer ordering across threads, reset,
timeouts, and under tokio (multi-thread and current-thread) as well as `block_on`.

## How the existing Styx code maps onto this

The existing backends live in `crates/styx/src/capture_api/` and are selected by `BackendKind`;
`probe_all` builds `ProbedDevice { identity, backends: Vec<ProbedBackend> }`, and each
`ProbedBackend` carries a `BackendHandle` and a `CaptureDescriptor` (a flat list of modes).

| Today | With providers |
|---|---|
| `BackendKind` | `Provider::name()` |
| `ProbedDevice.identity` (`DeviceIdentity`) | `DeviceInfo.identity`; merging stays keyed on shared fingerprints |
| `ProbedBackend { handle, descriptor, properties }` | `(provider, DeviceKey)` plus `DeviceInfo.graph`; modes become sink port capabilities |
| `Mode` + `Interval` | `StreamConfig { path, format, size, interval }` |
| `start_backend` (`dispatch.rs`) | `Provider::open` → `Device::configure` → `Device::start` |
| `CaptureHandle` frame queue | `OutputStream.frames` (async; blocking via `block_on`) |
| `watch` (inotify, libcamera hotplug) | `Provider::hotplug()` streams |

Per backend:

- **V4L2** (`v4l2_backend.rs`, `styx-v4l2` probing): becomes the native provider on
  `styx-kernel`. With a media device, the graph comes from `MEDIA_IOC_G_TOPOLOGY` (entities →
  nodes by function: sensor, CSI-2 receiver, ISP, video I/O → `Sink`; pads → ports; links as
  reported). Without one, a three-node graph `Sensor → Receiver → Sink` is synthesised from
  `VIDIOC_ENUM_FMT`/`ENUM_FRAMESIZES`/`ENUM_FRAMEINTERVALS`. The mmap/dma-buf queue and
  `DQBUF` loop become an `AsyncFd` on the video node with `readable()`.
- **UVC**: the same V4L2 provider (uvcvideo exposes a media device: camera terminal → `Sensor`,
  processing/extension units → `Other`/`IspStage`, streaming interface → `Receiver(Usb)`, video
  node → `Sink`). A userspace (usbfs) UVC provider later presents the same graph shape.
- **libcamera** (`libcamera_backend.rs`): the compat provider. libcamera hides the media graph,
  so it reports `Sensor → IspStage("libcamera") → Sink` per stream role (raw, main, low-res).
  `has_isp_second_output` (routes.rs) becomes "the path set contains two pixel paths through one
  `IspStage`", and `isp_output`/`isp_second_output` become a `StreamConfig` on the second sink
  with the scaled size. On the CM5 the native provider replaces it with the real PiSP graph.
- **netcam** (`netcam_backend.rs`): `Sensor (remote) → Receiver(Network) → Sink` with MJPG or
  H.264 memory formats and the URL as a property; no hotplug (or reachability events later).
- **replay** (`replay_backend.rs`): `Sensor (recorded) → Sink` with the recorded formats and
  rates; the pacing and loop settings are provider properties. Recorded statistics and
  parameters appear as `Stats`/`Params` sinks once recordings carry them.
- **virtual / file / simulation**: `Sensor → Sink` graphs; `MockProvider` is the test double
  for all of them.

### The planner

`planner/routes.rs` enumerates candidates as `(backend, mode, route)` where `Route` is
`Direct`, `LumaView` or `Decode { decoder, hardware }`, and costs them with `cost.rs`
constants. On graphs:

1. Candidates are `(device, path, sink format/size/interval)`: for each sensor, each pixel path
   from `output_paths`, each format and size the sink port accepts.
2. Routes beyond the sink are planned as software/hardware nodes appended through memory links:
   `Decode` is a `Codec` node (hardware) or `Compute(Cpu)` node (turbojpeg/zune), `LumaView`
   is a zero-cost view, pyramids and crops are `Compute(Cpu)` or ISP output sizes. Their cost
   hints carry today's constants (`MJPEG_LUMA_MS_PER_MP`, `HW_DECODE_*`, `BOX_LEVEL_MS_PER_MP`,
   `ISP_CAPTURE_LATENCY_MS`, the UVC latency model).
3. Ranking is unchanged: `score(total, priority)` over `GraphPath::cost` plus route costs.
4. `plan_many` (shared captures) picks one `StreamConfig` per distinct preparation and asks
   `link_plan` whether the paths can run together (two back end outputs: yes; two different
   feeds into `pispbe:input`: no). The existing pull-based fan-out (`session.rs`) stays on top
   of the per-output frame streams.
5. `PlanStep` descriptions can name graph nodes (`pispbe:output1 640x400`) instead of backend
   strings.

Nothing in `crates/styx` changes until the providers land; this crate only fixes the shapes.

## Open questions

- **Intra-node routing.** Paths assume any input of a node reaches any of its outputs. Subdevs
  with routing tables (`VIDIOC_SUBDEV_G_ROUTING`, multiplexed CSI-2 streams with embedded
  data) need per-node routes and stream ids on ports.
- **Async open/configure.** `discover`, `open`, `configure` and `start` are synchronous (they are
  quick ioctls, but starting a sensor through the bridge waits for an acknowledgement, bounded).
  Making them async needs boxed futures in the object-safe traits.
- **Per-frame metadata and controls** are not in the contract yet: the frame type is opaque,
  so the integration defines how sequence, timestamps, exposure and "which request produced
  this frame" travel (planned for the session runtime).
- **Buffer ownership across memory links** (who allocates, dma-buf import/export between
  `fe_image0` and `pispbe`) is described only by memory domains today; the session runtime will
  need buffer pools per memory link.
- **Formats across bus → memory boundaries** are not checked (e.g. `SGRBG10_1X10` → `BA10` or
  `PC1R`); a mapping table from bus codes to pixel formats would let `validate` check them.
- **Unsafe outside `styx-kernel`.** The reactor's syscalls (`epoll`, `eventfd`, `timerfd`,
  `fcntl`) live in `rt/sys.rs` with `// SAFETY:` comments. If the rule is strict, they move
  into `styx-kernel` (or use `rustix`) once crates may depend on each other.
- **One Frame type per registry.** Providers with different native frame types need adapters;
  whether `Provider` should instead be generic over a frame factory is open.
