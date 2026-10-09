# Changelog

All notable changes to this workspace should be documented in this file.

The format is based on Keep a Changelog and this project follows Semantic Versioning.

## [Unreleased]

### Added

- **Multi-camera frame grouping by sensor timestamp** (docs/multi-camera-sync.md).
  `styx_core::multicam` (`no_std` + `alloc`, no clock of its own): `Grouper<T>` groups frames
  of N cameras whose timestamps lie within `GroupConfig::tolerance_ns` into `FrameGroup`s
  (members, `reference_ns`, `spread_ns`, `complete`), under `GroupPolicy::{Strict, Partial,
  Latest}` and `RateMatch::{Nearest, Slowest}` (cameras at different rates paired to the
  slowest), with a deadline for late cameras, at most `depth` frames per camera and
  `output_depth` groups held (dropped frames are released at once), cameras connecting,
  disconnecting and being removed, and every drop counted by `DropReason`. `SyncStats` /
  `SyncReport`: spread per group and p50/p99/max over the last 256 groups, each camera's offset
  to a reference camera and its drift in ppm (`DriftEstimator`, exponentially weighted least
  squares), frame period, match rate, drops by reason per camera.
  `styx::multicam::FrameGrouper` (Linux): sources `CaptureHandle`, `MediaPipeline`,
  `FrameClient` (its `Connected`/`Disconnected` events mark the camera present or absent),
  `BoundedRx<FrameLease>` or any `GroupSource`; timestamps put on one clock
  (`ClockMode::Common(clock)` converting monotonic/boottime/realtime, `Arrival` for sources
  without a sensor clock, `Raw`), frames on clocks that cannot be related refused with
  `ClockError`; `try_next`, `recv(wait)`, `try_event`/`recv_event` (`GroupEvent`),
  `poll_next`, `next().await`, `stream()`, and an `AsFd` descriptor readable when it has work
  (eventfd waker for in-process queues, timerfd for deadlines, the clients' descriptors).
  `CaptureHandle::poll_recv(cx)` and `MediaPipeline::poll_next(cx)` are new for it.
- **Sync quality metrics**: named groupers (`FrameGrouper::named`) are listed in
  `styx::metrics::snapshot().sync_groups` (`SyncGroupMetrics`, `styx::metrics::sync_groups()`)
  and exported as `styx_sync_groups_total{kind}`, `styx_sync_match_ratio`,
  `styx_sync_spread_ms{quantile}`, `styx_sync_frames_total{camera,stage}`,
  `styx_sync_drops_total{camera,reason}`, `styx_sync_offset_ms`, `styx_sync_offset_mean_ms`,
  `styx_sync_drift_ppm`, `styx_sync_frame_period_ms`, `styx_sync_camera_connected` (label
  `group`; docs/metrics.md). `MetricsSnapshot` gained `sync_groups` (serde default; the
  `styx::ipc` wire version is unchanged). `metrics-serde` now enables `styx-core/serde`.
- **Frame groups into Daedalus as one tick** (`styx_core::daedalus`, feature `daedalus`):
  `push_group(host, &GroupPorts, group)` pushes each camera's frame (`frame_payload`, no copy)
  to its port, and optionally a `FrameGroupInfo` (`styx:frame_group`: sequence, reference,
  spread, cameras present, offsets; registered by `StyxFramesPlugin`), as one atomic
  `HostBridgeHandle::push_batch`, so a tick sees a whole group or none of it;
  `group_payloads` builds the batch. Example `daedalus_multicam`.
- docs/multi-camera-sync.md: the grouper, clocks per backend, metrics, Daedalus, and hardware
  sync of the OV9782/OV9281/OV9282 (FSIN trigger and strobe registers from the RPi `ov9282`
  driver, what needs the datasheet, what a CM5 carrier needs, a proposed `[sync]` sensor
  description section, expected software vs hardware sync quality).
- `styx-record --raw`: record the sensor's raw 8-bit stream (R8/GREY or 8-bit Bayer) instead
  of the ISP's Y plane. The settings file's `source_stream` (and a still's `.txt`) says what the
  bytes are: `NV12 Y plane via ISP`, `raw R8`, `GREY`, ...
- `ControlPlane::get_control` / `set_control` and `CaptureHandle::control_plane()`: read and set
  a capture's controls from another thread.
- `styx-record` (`tools/styx-record`, docs/recording.md): grey test recordings for Eidos. The
  Y plane at the camera's native size in Eidos's raw grey video layout
  (`<name>_<W>x<H>_gray.raw`: frames back to back, no header), a per-frame sidecar
  (`<name>.frames.csv`: sequence, sensor timestamp, clock, frames dropped before each, from
  sequence numbers or else timestamps) and the camera settings (`<name>.json`: exposure,
  analogue gain, frame rate and duration, size, format, camera, AE/AWB, every control and its
  changes while recording, Styx commit, start and end time, SHA-256 of the raw file). From a
  Styx camera service as a normal `FrameClient` (no capture restart for the other clients;
  `--mode every` reporting drops, or `--mode latest`), or `--source direct` through
  libcamera (feature `libcamera`) or the native stack where no service runs, with a clear
  error when another process holds the camera. Stills for calibration (`--stills N --on-key`
  or `--every-secs K`) in Eidos's live-capture layout (`.gray.raw`, `.pgm`, `.txt`). Ctrl-C
  finalises the files; the disk writer has its own thread and a bounded queue and reports when
  the disk does not keep up. No codecs; without features it links only glibc and libgcc_s.
- Camera previews for user interfaces: `styx::preview` (feature `preview`, Linux;
  docs/preview.md). `Preview::from_service(ClientOptions, PreviewConfig)` (a low-priority,
  reconnecting camera service client, connected only while someone watches) or `Preview::new`
  with `Preview::offer(&FrameLease)` / `offer_owned`; `PreviewConfig` (`size`, `max_fps`,
  `quality`, `encoder: JpegBackend`, `nice`, `encode_unwatched`, `passthrough_jpeg`,
  `idle_disconnect`, `with_request`). Encoding on a thread of its own (niced to 10 by
  default): the frame or its smallest covering companion is scaled to planar YUV 4:2:0 (2x2
  box halvings with `styx_core::simd::box2_row`, then one bilinear pass; NV12, NV21, I420,
  YV12, YUYV, UYVY, grey, RGB24/BGR24) and released before encoding; camera MJPEG passes
  through. Latest-frame semantics: one frame waits at most (`dropped_busy`), frames over the
  cap are dropped before any work (`dropped_rate`), viewers (`PreviewSubscriber`: `recv`,
  `try_recv`, `next().await`, `Stream`) get the newest JPEG (`PreviewFrame`: `jpeg: Bytes`,
  `sequence`, `timestamp_ns`, `clock`, `width`, `height`, `gray`, `passthrough`, `encode_us`).
  Serving without an HTTP framework: `MjpegStream` (`multipart/x-mixed-replace` as a `Stream`
  of `Bytes`; `fallible()` for hyper/axum; `MJPEG_CONTENT_TYPE`), `ws_message` /
  `parse_ws_message` (`SPV1`: a 32-byte header with sequence, timestamp, clock, size, flags,
  then the JPEG). New optional dependency: `bytes`.
- `styx_codec::jpeg_planar`: `PlanarJpegEncoder` encodes planar YUV 4:2:0 or grey
  (`JpegInput`) with `JpegBackend::{Turbojpeg (YUV planes as they are), Mozjpeg (raw data),
  Image (pure Rust)}`, keeping the encoder and its buffers between images.
- Low-priority camera service clients: `ClientOptions::low_priority()` / `priority(..)`,
  `ClientPriority::{Normal, Low}`. The service plans normal clients as if low-priority ones
  were not there; a low-priority client gets its own request only when that leaves every
  normal client's plan as it is (`same_plan`: mode, rate, route, ISP outputs, regions) and adds
  no CPU step to the service, else a share of a normal client's frames; it never restarts a
  capture normal clients use (refused instead), cannot set a frame rate that restarts it, and
  is never the camera's owner while a normal client is connected. Protocol: a trailer on the
  request message, ignored by older services; the version stays 8.
- Preview metrics: `PreviewMetrics` in `MetricsSnapshot::previews` and Prometheus
  `styx_preview_*` (frames in, encoded, dropped by cause, bytes, encode/scale/capture-to-encoded
  windows, preview thread CPU from `CLOCK_THREAD_CPUTIME_ID`, subscribers; docs/metrics.md).
- Examples (feature `preview` of `styx-examples`): `preview_server` (a virtual or real camera
  through a camera service, a vision client, the preview served by a tiny std HTTP server:
  MJPEG, latest JPEG, the WebSocket message, metrics; measurements each second) and
  `preview_encode_bench` (encode cost per encoder, size, quality and input; the whole preview
  path).

- Frame socket: each frame once per consumer, without polling. The sibling endpoint
  `<path>.next` (`frame_socket::next_path`, module `frame_socket::next`) answers a consumer's
  line `"<timestamp>\n"` (the frame it has, `0` for none) with the first frame published whose
  timestamp differs, as soon as there is one; same message and lease as the frame socket.
  `FrameFetcher::fetch_next(wait)` uses it (falling back to fetching from the frame socket
  until the frame is new against an older server) and `FrameFetcher::stats()`
  (`FetchStats { fetched, repeated, polled }`) counts what it fetched. `FrameSocketStats` and
  `FrameSocketMetrics` gain `served_frames` (distinct frames sent) and `repeated` (sends of a
  frame already sent, on the frame socket itself); Prometheus `styx_frame_socket_events_total`
  gains `event="served_frames"` and `event="repeated"`. The lease message (`styx-frame-lease-v1`)
  is unchanged; the frame socket itself behaves as before.

- Connection changes, in order with what a client receives (docs/frame-server.md "Connection
  changes"): `FrameClient::try_client_event()` and `ControlClient::try_client_event()` return
  `RecvOutcome<ClientEvent<T>>` (`T`: `FrameLease`, `ControlEvent`), with
  `ClientEvent::Connected { reconnects }` (the first connection, also after a blocking connect,
  and each reconnection), `ClientEvent::Disconnected { error }` (the connection lost, or a client
  that does not reconnect gave up; then `Closed`) and `ClientEvent::Data(T)`; each change once,
  waking the client's descriptor and waker; `poll_client_event(cx)`,
  `next_client_event().await` (`NextClientEvent`) and `client_events()` (`ClientEventStream`).
  Additive: `try_next`/`stream` and `try_event`/`events` are unchanged and return no connection
  changes. A client re-applies persisted settings on `Connected` without checking
  `is_connected()` (the `service_controls` example does).
- `ControlClient`: a camera service client for controls only (docs/frame-server.md "Control
  clients"). `ControlClient::connect(path)`, `connect_camera(path, camera)`,
  `ControlClient::options(path)` (`ClientOptions::controls()`, `controls_nonblocking()`,
  `.reconnecting()`). The same control methods as `FrameClient` (`set_control`,
  `get_control`, `controls`, typed setters) and events: an `AsFd` descriptor readable when
  `try_event()` has a change or news, `recv_event(wait)`, `poll_event(cx)`,
  `next_event().await` (`NextEvent`), `events()` (`ControlEventStream`). It makes no frame
  request: it never joins the camera's frame plan, never starts, restarts or stops a capture
  by connecting or leaving, holds no buffers, and is not counted among the service's clients
  or metrics; `ControlPolicy` applies to it (never the owner, `ControlCaller::client: None`).
  No protocol change: it sends the existing token-less control requests; the service now
  looks their camera up once per control connection instead of per request.
- Async control requests on any executor, for `FrameClient` and `ControlClient`:
  `set_control_async`, `get_control_async`, `controls_async` (styx-graph's reactor, never
  blocking a thread).
- Connecting without blocking: `ClientOptions::request_nonblocking(&request)` returns a
  `FrameClient` at once, before (or without) the service; it connects in steps driven by its
  descriptor (`try_next` returns `Empty` until frames arrive, `recv(wait)` never waits longer
  than `wait`), `ready().await` (`Ready`) / `poll_ready(cx)` wait for the connection on any
  executor, `last_error()` says why it is not connected. Reconnecting, it keeps trying until
  the service and camera are there; otherwise it gives up after the timeout or a rejection
  (`Closed`). The blocking `request` is unchanged.
- Camera controls over a camera service's socket (docs/frame-server.md "Camera controls"):
  `FrameClient::set_control(id or StandardControl, value) -> AppliedControl` (the value in
  effect, whether it was clamped, the first frame using it on frame-exact backends, deferred
  while the camera is stopped, restarted for a frame rate), `get_control`, `controls()`
  (`ControlDescriptor`: the backend's `ControlMeta`, the value now, the `StandardControl` it
  answers, whether this client may change it), typed helpers (`set_exposure_us`, `set_gain`,
  `set_ae`, `set_ev`, `set_fps`, `set_awb`, `set_colour_temperature`, `set_colour_gains`,
  `set_af_mode`, `trigger_af`, `cancel_af`, `set_lens_position`) and
  `FrameClient::control_events()` (`ControlEvents`: every accepted change on the camera, with
  the client that made it, `FrameClient::client_id`). Refusals are
  `IpcError::ControlRefused(ControlRefusal::{Unsupported, ReadOnly, Invalid, NotPermitted,
  Failed})`. `StandardControl`s map to native and virtual cameras' controls by name, to
  libcamera's by name and to V4L2/UVC ids with unit conversion. A frame rate the camera cannot
  change while streaming restarts the capture for every client at that rate
  (`SERVICE_FRAME_RATE`). Controls set are applied again after every capture restart.
  `CameraService::control_policy(ControlPolicy)`: any client by default (last write wins,
  everyone subscribed told), `owner_only()`, `read_only()`, `read_only_control(..)`,
  `allow(|caller, id| ..)`, `no_restart()`. New message kinds Control, ControlReply,
  ControlEvent and an accept trailer with the client's id and token; protocol version 8
  unchanged (old clients never get the new messages). Fuzzed (`ipc_messages`, new seeds).
- `FrameClient` without a thread of its own: `AsFd`/`AsRawFd` (an epoll descriptor readable
  when there is a frame or news, stable across reconnections), `try_next()` (never blocks, a
  reconnecting client reconnects without blocking too), and runtime-agnostic
  `poll_next(cx)`, `next().await` (`NextFrame`) and `stream()` (`FrameStream`, a
  `futures_core::Stream`) waking through styx-graph's reactor (one thread per process).
  Steady state unchanged: one allocation per frame (its release record), zero copies
  (`tests/zero_alloc.rs` covers poll + `try_next` and `next().await`, and the reactor thread).
  `examples/05_apps/camera_service_async.rs` takes several cameras' frames on one thread, both
  ways; `camera_service controls` lists or sets a camera's controls.
- `styx_graph::rt::AsyncFd::poll_ready` / `poll_read_ready` for hand-written futures.
- Virtual cameras with controls: `capture_api::make_virtual_device_with_controls`
  (`ControlPlane::VirtualControls`, `VirtualControls`: values checked, kept and read back).
  `CaptureHandle::set_control_landing` (the first frame using the value on a native camera's
  raw modes) and `CaptureHandle::control_metas`.
- Added per-frame hop timestamps and frame path counters (docs/metrics.md "Hops", "Copies,
  dma-buf syncs, exhausted pools"): `FrameMeta::hops` (`styx_core::buffer::FrameHops`: fixed
  size, `Copy`, no allocation; empty without the new styx-core feature `path-metrics`, which
  `std` enables) with `Hop::{Sensor, Dequeued, IspDone, Queued, Taken, Sent, Received,
  Imported}` in `CLOCK_MONOTONIC` ns, the copies made of the frame and the sequence number
  (`FrameMeta::sequence` falls back to it); `FrameMeta::hop_record()` / `HopRecord` (serde) as
  the join key for other tools. Native PiSP and software ISP captures stamp dequeue and ISP
  done, every capture its queue time, `recv*` the take; `styx_core::metrics::HopCounters`
  windows them per capture (`CameraMetrics::path`, a `HopMetrics`), per frame socket, per
  camera service client (`ConsumerMetrics::hops`) and per consumer
  (`FrameFetcher::hop_metrics`, `FrameClient::hop_metrics`). Copies are counted at every
  copy site through one helper (`styx_core::metrics::copied_frame` / `copied`, by `CopySite`),
  `DMA_BUF_IOCTL_SYNC` calls with their time, and exhausted pools: `ProcessMetrics::path`
  (`PathMetrics`, `styx::metrics::path()`). `Window::p99_ms`; Prometheus `styx_camera_hop_ms`,
  `styx_camera_path_*`, `styx_consumer_hop_ms`, `styx_process_copies_total{site}`,
  `styx_process_dmabuf_sync*`, `styx_process_pool_exhausted_total`, quantile 0.99.
  `metrics_top` shows them (and `--frame-socket PATH`); `metrics_top --overhead` also measures
  the hops; `hop_breakdown` (example) prints a frame's path hop by hop in-process, through a
  frame socket and through a camera service, with copies, syncs and allocations per frame.
- The frame lease message carries the frame's hops as an optional last member `"hops"`
  (`LeaseMessage::hops`, `Option<HopRecord>`): left out for frames without hops (the bytes are
  then the same as before, golden-tested), ignored by older peers. `FrameSocket` adds the send
  time, `fetch_frame` / `FrameFetcher` the receive and import times. The camera service's frame
  message carries the hops and its release message the client's receive and import times, as
  trailers older peers ignore (protocol version unchanged).
- Added `FrameSocket::metrics()` (`FrameSocketMetrics`: counters, lease hold times, hops of
  the frames sent, the process snapshot) and the statistics endpoint `<path>.stats`
  (`frame_socket::stats_path`, `fetch_metrics`, `fetch_metrics_text`): JSON or Prometheus text
  for a profiler polling a running frame socket.
- Added `FrameFetcher`: fetches from a frame socket again and again with its buffers and the
  mapping of each frame buffer kept (a fetch maps nothing in steady state and allocates one
  record, the frame's lease); `lease_codec::LeaseMessageInline` parses and checks a message
  without allocating; `LeaseMessageRef` / `EncodedFrame` / `write_framed` write one into a
  reused buffer. `ExternalBacking::export_into` / `FrameLease::export_backing_into` /
  `export_or_copy_memfd_into` export into the sender's list (`ExportedKind`).

- Added a public `styx-frame-lease-v1` codec without a transport: `styx_core::lease_codec`
  (styx-core feature `lease-codec`, unix; re-exported as `styx::ipc::lease_codec`) with
  `LeaseMessage { descriptor, backing }`, `LeaseBacking::{Memfd { len }, DmabufPlanes { planes }}`,
  `LeasePlane { offset, len }` (the frame socket's JSON, unchanged and golden-tested), the
  descriptor order (memfd: one; dma-buf planes: one per plane, in order), `MAX_FDS`,
  `MAX_PAYLOAD`, `TRANSPORT`, `HEADER_LEN`, `encode` / `encode_framed` / `payload_len` /
  `decode` / `decode_message` with a typed `LeaseCodecError` (descriptor count, payload size,
  planes within their descriptors, a memfd's `len` within the memfd), and the endpoint record
  helpers `endpoint_uri`, `endpoint_payload`, `parse_endpoint_uri`, `parse_endpoint`
  (`styx-frame-lease+unix://<path>`). `FrameSocket` and `fetch_frame` use it; codec errors are
  `IpcError::Lease`. The `frame_socket_message` fuzz target decodes through it. The frame
  socket's hold, release, expiry, every-buffer-held, sizing and ordering rules are documented
  (module docs, `docs/frame-server.md`) and each is a host test.
- Added writable frames over caller-provided memory: `MemoryRegion::from_raw_mut` /
  `from_static_mut` make a region a frame writes in place (`planes_mut`, the new
  `FrameLease::plane_data_mut`, `visible_rows_mut` and the visible-plane copies) while it is the
  region's one owner, without `std`. `RegionHooks::begin_cpu_write` runs before the first write
  and `end_cpu_write` once the writes are done (the frame is shared, its backing handed out,
  `FrameLease::finish_cpu_write`, or dropped, before `release`); on Linux
  `dmabuf_begin_cpu_write` / `dmabuf_end_cpu_write` are the dma-buf syncs for them. Other
  backings opt in through `ExternalBacking::{cpu_writable, plane_data_mut, finish_cpu_write}`.
- Made `styx_core::math` public: `Float` (sealed), std's float methods for `f32` / `f64`
  without `std` through `libm` under std's names (std builds still call the inherent methods),
  now also `div_euclid`, `ln_1p`, `cbrt`, `tan`, `asin`, `acos`, `atan` and `tanh`. Exact
  functions match std bit for bit, transcendental ones within 1 ulp (`tanh` 3), tested
  against std for `f32` and `f64`.
- Added the rest of the atomics to `styx_core::sync`: `AtomicI8`, `AtomicI16`, `AtomicI32`,
  `AtomicI64`, `AtomicIsize`, `AtomicU16`, `AtomicPtr`, `fence`, `compiler_fence`, and `Weak`
  next to `Arc`, from the same source as before (`portable-atomic`, critical sections with
  `critical-section`), checked on every `no_std` target including Cortex-M0.
- Added several regions of interest per consumer: `FrameRequest::regions([..])` (up to
  `MAX_REGIONS`, 16; region 0 is the frame, the others `CompanionKind::Region { index }`
  companions, `FrameLease::region` / `regions`), moved with `RoiHandle::set_regions` /
  `set_region` (`FrameClient::set_regions`), and `FrameRequest::skip_stale_regions` to skip
  frames the ISP cropped before a region moved. On a native PiSP every consumer of a shared
  capture gets ISP-cropped regions: the main output's crop (when no consumer needs the whole
  frame), the second output's, or extra back end passes over the same raw frame (`RoiCrop::IspPass`;
  `Delivered::regions`); luma regions of a whole frame are views. Without a PiSP the overview is
  box-filtered to the size asked for. Below the planner: `PispPipeline::set_pass`
  (`styx_pipeline::pisp_passes`: `PassSpec`, `PassTdn`, `PispFrame::passes`),
  `NativeIspConfig::regions` (`NativeRegion`, `StyxConfig::native_regions`) with the
  `region_crop(index)` controls, a pyramid level from an extra pass when the second output makes
  the overview or the main output is cropped. Camera service wire format v8.
- Added regions of interest and overviews on the native software ISP: the ISP processes only
  the region at full resolution (bit-exact against the whole frame's crop) and bins the whole
  frame for the overview, gathering the whole frame's statistics there (or in a pass of their
  own), so AE and AWB are unchanged and a small region costs a fraction of a frame (CM5,
  1280x800: 3.2 ms CPU per frame whole, 1.3 ms for a 320x200 region with a 320x200 overview).
  The planner plans `RoiCrop::Isp` on it and prices the capture for the region and overview;
  `OUTPUT_CROP` is offered on software ISP cameras. `styx-softisp`: `SoftIsp::process_window`,
  `window_size`, `process_binned`, `binned_size`, `statistics` and `Window`. `styx-pipeline`:
  `SoftTarget`, `SoftParts`, `SoftLoop::process_target_with`, `SoftPipeline::next_target`.
  `native-pipeline soft --roi / --overview / --check-roi / --cold`.
- Added libcamera regions of interest on a Raspberry Pi ISP: ROI plans crop per output with
  `rpi::ScalerCrops` (the main output the region at the first region's size, the second the
  overview), moved with `OUTPUT_CROP` (`Frames::roi`); frames carry `FrameMeta::crop` from the
  request metadata. `LibcameraConfig::crop` / `overview`, `StyxConfig::libcamera_crop`.
- Added ISP-cropped regions of interest and overviews on native cameras with a PiSP: with
  `FrameRequest::roi` the back end's main output is the region at full resolution, in any format
  (moved per frame with `Frames::roi` / `FrameClient::set_roi`), and `FrameRequest::overview(w, h)`
  attaches the whole frame scaled down from the second output (`CompanionKind::Overview`,
  `FrameLease::overview`; crops keep it whole). Elsewhere the overview is the uncropped frame.
  `Delivered` says how (`roi: Option<RoiCrop>`, `overview`, `hardware_overview`), with
  `Unmet::Roi` and `Unmet::Overview`. Below the planner: the native `OUTPUT_CROP` control,
  `NativeIspConfig::crop` / `overview` (`StyxConfig::native_crop` / `native_overview`),
  `PispPipeline::set_output_crop` and `PispOptions::crop`. Camera service wire format v7.
- Added the `daedalus` feature to `styx-core-rs` (and `styx`, passed through): Styx frames in
  Daedalus graphs from one place, `styx_core::daedalus`. It provides the `styx:framelease` type
  key and `FrameLease` type, `FrameDescriptor` (format, size, planes, timestamp, residency, CPU
  access, companions), a `MetadataOnly` frame-to-descriptor adapter, an inspection serializer,
  all registered by `StyxFramesPlugin`, and zero-copy `frame_payload` with residency mapping.
  See `docs/daedalus.md` and the `daedalus_frames` example.
  Daedalus is its `dev` branch by git, now Daedalus 3.0.0 (a local `[patch]` overrides it).
- Added Daedalus's generic frame view for Styx frames: `FrameLease` implements
  `daedalus:frame` v2 (`FrameSource`; Daedalus `dev` at `ed7ddde`, which v1 builds and plugins
  refuse), registered by `StyxFramesPlugin` as a provider
  (`foreign_providers(FrameLease => FrameInterface)`, adapter
  `daedalus.foreign:styx:framelease->daedalus:frame`), so nodes taking `FrameView<'_>`,
  including separately built plugins that know nothing of Styx, read a frame in place. Plane
  metadata never maps or syncs: DRM fourcc, modifier and format kind (`Pixel`; `Bayer` for
  libcamera's raw codes, with `MIPI_FORMAT_MOD_CSI2_PACKED` when CSI-2 packed; `Compressed`
  with the V4L2 fourcc, e.g. `MJPG`; `Unknown` without a DRM code), `u64` offsets, strides and
  lengths, borrowed dma-buf descriptors, and the plane mapping (`Cached`, `Uncached`,
  `Unmapped` from `CpuAccess`). Bytes come from `plane_data` / `end_cpu_access`
  (`FrameLease::begin_cpu_read` / `end_cpu_read`): mapped on first use, dma-bufs synced while
  read; none for planes the CPU cannot read. `styx_core::daedalus::{frame_view,
  view_residency, plane_mapping, format_kind, FrameView, FrameInterface, FrameFormatKind,
  PlaneMapping}`. `shared_frame_payload` finishes open CPU writes first. Plugin id, type keys
  and existing adapter ids unchanged. `zero_alloc` covers the view: no copy, one allocation
  per received frame as before, and an fd-only consumer of dma-buf frames makes no `mmap`, no
  `plane_data` call and no dma-buf sync.
- Added bracketed CPU reads of frames: `FrameLease::begin_cpu_read` / `end_cpu_read` and
  `ExternalBacking::begin_cpu_read` / `end_cpu_read` (default: `plane_data`, nothing to end),
  counted by `styx_core::buffer::CpuReadWindow`: the first open read starts CPU access
  (`DMA_BUF_IOCTL_SYNC` START), the last ends it, while plain reads (`planes()`) still hold it
  until the frame drops. The shared-fd, IPC (`CachedDmabuf`), libcamera, PiSP and native
  capture backings map lazily as before and sync through it (their wrappers forward it); the
  native runtime's `Lease` / `Frame` gain `begin_read` / `end_read`. Path metrics count
  `frame_maps` (`mmap`s made to read frames) and `cpu_reads`.
- Added `styx_core::format::drm` (`no_std`): `to_drm(FourCc) -> Option<DrmFormat>` and
  `from_drm(fourcc, modifier) -> Option<FourCc>` over a public table (`MAPPINGS`, `ALIASES`,
  `UNMAPPED`) covering every pixel format Styx defines: greyscale, packed RGB (Styx `RG24` =
  V4L2 `RGB3` is DRM `BG24`, and the other byte-order differences), packed, semi-planar and
  planar YUV, and Bayer and CSI-2 packed raw in libcamera's DRM extension
  (`MIPI_FORMAT_MOD_CSI2_PACKED`, `DrmRegistry::Libcamera`); compressed formats have none.
- Added `ExternalBacking::dmabuf_plane` and `FrameLease::dmabuf_plane` (`DmabufPlane`: a
  plane's dma-buf, borrowed, and its offset in it) for handing a plane to a device by
  descriptor without duplicating it, reported by the PiSP, libcamera, native capture
  (`LeaseBuffer::dmabuf`), imported, received IPC and shared-fd backings; and
  `FrameLease::plane_at` (one plane without building the others).
- Added `CpuAccess` (`None`, `Uncached`, `Cached`), `ExternalBacking::cpu_access` and
  `FrameLease::cpu_access`: whether the CPU can read a frame's planes and how fast, apart from
  its residency. Native ISP and sensor buffers, libcamera buffers, IPC frames (as the sender
  reports) and readable DRM-PRIME frames say how they read; other dma-buf backings are not
  readable unless they opt in. Adds `Codec::handles_companions`.
- Added `FramePlan::delivered()` (`Delivered`: format, size, frame rate, pyramid levels and the
  hardware one, inter-coding, and `unmet` requirements as `Unmet`) and `FramePlan::unmet`;
  `FrameClient::delivered()` carries it from the camera service's answer. `FrameRequest::strict`
  refuses plans that miss part of the request (planning error, or a camera service rejection).
- Added `FrameClient::options` (`ClientOptions`: camera, timeout, reconnecting) with a timeout for
  connecting and the service's answer together (`DEFAULT_OPEN_TIMEOUT`, 10 s) and
  `request_async`, which awaits the connection without blocking and gives up when dropped.
- Added autofocus for native cameras with a focus lens (simulated; hardware validation
  pending): AF in `styx-algo` (PDAF loop and contrast scans from Raspberry Pi's `af.cpp`, frame-
  exact scan steps, `rpi.af` tuning import, `sim::FocusSim`), focus lenses as data in
  `styx-sensor` (kernel lens drivers via `FOCUS_ABSOLUTE`, DW9714/DW9807/DW9817/AK7375 VCMs on
  I²C, IMX708 PDAF), frame-exact lens moves with per-frame reported positions in `styx-native`
  (`FrameControls::lens`), software ISP focus statistics, and the processed native controls
  `AF_MODE`, `AF_TRIGGER`, `AF_STATE`, `LENS_POSITION`, `AF_WINDOWS`, `AF_METERING`,
  `AF_RANGE`, `AF_SPEED` (continuous AF by default).
- Added `Frames` / `FrameRequest`, the general "what frames do I want" API:
  `Frames::nv12().size(1280, 800).fps(30).open(&camera)?` (also `rgb()`, `gray()`,
  `formats([..])`, `any()`, `open_best`, and `camera.frames()...open()`). One meaning per choice:
  size (`size`, `size_at_least`, `size_at_most`), frame rate (`fps` exactly, with an error naming
  the available rates; `fps_at_least`; `fps_between`; none for the camera's default), delivery
  (`latest()` newest-only, `every_frame(n)` queued with drops counted by `Frames::dropped`),
  advanced options (`pyramid`, `roi`, `row_alignment`) and route control (`backend`,
  `hardware(Hardware::..)`, `decoder`, `forbid`, `decode_threads`). The opened stream is the
  planner's frame stream (`PlannedFrames`, renamed `Frames`), now with `set_control`,
  `get_control`, `capture()` and `dropped()`; `FramePlan::config` passes capture settings.
- Changed: the planner ranks routes by one cost (CPU + latency / 2, printed in every plan)
  instead of by `Priority`; queue depth and decode threads follow the delivery. Without a rate,
  list-rate (USB) cameras run at the listed rate closest to 30 fps instead of their fastest.
  `FramePlan::requirements` is now `FramePlan::request`. The camera service wire format is
  version 4 (carries a `FrameRequest`).
- Deprecated: `FrameRequirements`, `Priority`, `PlanOverrides` and `HardwarePolicy` (styx-core)
  and the `PlannedFrames` name; planner functions and `FrameClient::request` still accept
  `FrameRequirements`, converted with `FrameRequest::from`. `styxsrc`'s `priority` property is
  deprecated in favour of `queue-depth`.
- Added frame planning: `FrameRequirements` (styx-core) describes what a consumer needs (format,
  stride alignment, pyramid, ROI, resolution/fps bounds, priority, overrides) and
  `styx::planner` picks the backend, mode and route, explains the plan, and runs it with a live
  ROI handle. See `docs/frame-planning.md`.
- Added zero-copy ROI views (`FrameLease::crop_view`, `FrameMeta::crop`), per-frame latency
  breakdowns (`FrameMeta::latency`, `FrameTiming`), pooled pyramid buffers
  (`with_box_pyramid_in`), `CodecRegistryHandle::lookup_for_output[_where]` and
  `MediaPipelineBuilder::decoder_for_output`, and whole-frame byte transfer
  (`FrameLease::from_visible_bytes`, `to_visible_vec`, `MediaFormat::with_default_color`).
- Pyramid companions now survive pipeline decode and rotate/mirror stages.
- libcamera buffer mappings are kept for the capture session (0.38 → 0.15 ms per Y-plane read on
  a CM5), and sensor latency uses libcamera's `SensorTimestamp`.
- Added MJPEG luma criterion benchmarks and a perf-smoke metric on real Logitech C270 frames.
- Added timestamp clocks: `FrameMeta::clock` records which clock `timestamp` is in
  (`Monotonic`, `Boottime`, `Realtime` or `StreamRelative`), `FrameMeta::timestamp_in` converts
  between system clocks, and `StyxConfig::timestamp_clock(ClockSource::...)` makes live sources
  (libcamera, V4L2, netcam, virtual) stamp frames in one clock. File and simulation sources keep
  media time. See `docs/timestamps.md`.
- Added `FrameDropReason::SensorSequenceGap`: health reports now count frames the sensor produced
  that never reached Styx (V4L2 sequence numbers; libcamera sensor timestamps and
  `FrameDuration`, since its sequence numbers count completed requests).
- Added `QueueOverflow` and `bounded_with` (styx-core), `CaptureConfig::queue_overflow`,
  `CaptureConfig::extra_buffers`, `StyxConfig::latest_frame_only()` and
  `FrameDropReason::CaptureQueueEviction`. See `docs/latest-frames.md`.
- Added camera disconnect recovery for libcamera and V4L2 captures (`ReconnectPolicy`, on by
  default): the handle stays open, the camera is found again by identity keys and restarted
  with the same mode, config and controls. Adds `CaptureError::Disconnected`,
  `CaptureRetryStats::{reconnects, last_reconnect_downtime_ms}`, `QueueStats::sent` and
  `ControlPlane::Supervised`. See `docs/reconnect.md`.
- Added lossless recording and replay: `StreamRecorder` writes frames with their metadata
  (pixels or bitstream, timestamp and clock, backend sequence numbers, crop, timing, pyramid
  companions), and `CaptureRequest::replay_source` plays them back as a camera
  (`BackendKind::Replay`, real-time or unpaced, optional looping). Recordings are MCAP files
  with ROS 2 message types (feature `replay-mcap`, default) that open in Foxglove and ROS 2
  tools. A compact Styx-only `.styxrec` format is available as an experimental opt-in
  (feature `replay-styxrec`). See `docs/replay.md`.

- Added cached dma-heap capture buffers for libcamera (`LibcameraBufferMemory`, default `Auto`:
  on for Raspberry Pi cameras, falling back to libcamera's allocator). libcamera's own PiSP
  buffers are mapped uncached; on a CM5 this makes CPU reads of a 1280x800 Y plane 1.7x faster
  for row scans and 3.8x faster for strided access. Override with `STYX_LIBCAMERA_BUFFER_MEMORY`.
- Added `DMA_BUF_IOCTL_SYNC` read bracketing for libcamera and imported dma-buf frames
  (`dmabuf_begin_cpu_read`/`dmabuf_end_cpu_read`), required for coherent CPU reads of cached
  buffers written by an ISP.
- Added zero-copy luma access: `FrameLease::luma_rows`, `into_luma` and `has_luma_plane` expose
  plane 0 of GREY/R8 and planar/semi-planar YUV frames (including mapped dma-buf frames) as Y8.
- Added GREY/R8 capture on libcamera colour sensors: when the sensor cannot produce 8-bit mono,
  Styx captures processed YUV420 and delivers its Y plane as a zero-copy GREY frame.
- Added pyramid companion frames on `FrameLease` (`CompanionKind`, `with_companion`,
  `pyramid_level`, `with_box_pyramid`, `box_downscale_luma`). libcamera can fill them from the
  ISP's second output in the same request (`CaptureRequest::luma_pyramid`,
  `StyxConfig::libcamera_pyramid_level`), so companions share the primary's timestamp.
- Added `TurbojpegLumaDecoder` (MJPG → GREY) with 64-byte-aligned rows, optional DCT scaling,
  cropping, fast IDCT and software pyramid levels. On a CM5 it decodes 1280x800 camera frames in
  2.4 ms versus 13.2 ms for the previous `jpeg-decoder` RGB path.
- Added `LibcameraFrameMeta` (sequence number and buffer memory) and `FrameMeta::sequence()`.
- Added multi-core MJPEG decoding to `TurbojpegLumaDecoder` (`LumaDecodeOptions::threads`, off by
  default): frames with restart markers are split into standalone slices decoded in parallel,
  byte-identical to a single-threaded decode. Logitech C270 720p on a CM5: 1.59 → 0.88 ms.
- Added FFmpeg hardware decode: `FfmpegHwDevice` (VA-API, CUDA/NVDEC, QSV, DRM),
  `FfmpegVideoDecoder::with_hw_device`, and `FfmpegLumaDecoder` (MJPEG/H.264/H.265 → GREY) with
  `best_available()` probing named SoC decoders (`*_rkmpp`, `*_v4l2m2m`, `*_nvv4l2dec`, ...) then
  devices. DRM-PRIME frames are now CPU-readable (mapped with dma-buf sync; tiled/compressed
  layouts stay export-only), and decoders that expose DRM-PRIME internally (ffmpeg-rockchip)
  get zero-copy output.
- Added rectangle controls (`ControlKind::Rectangle`, `ControlValue::Rect`/`Rects`,
  `ControlRect`): libcamera `ScalerCrop` and per-output `ScalerCrops` can now be set and read
  back.
- V4L2 capture now delivers NV12/NV21/NV16/I420/YV12 as proper multi-plane frames (zero-copy when
  the buffer layout allows).

- Added a lightweight `styx` `framelease` feature for crates that only need
  `FrameLease` and its `styx-core` requirements without enabling capture,
  codec, backend, service, graph, or preview modules.
- Added generic frame layout metadata in `styx-core-rs` for CV and media consumers, including
  storage kind, channel order, bit depth, chroma subsampling, packed pixel schema, plane schema,
  and `FourCc::layout_info()`.
- Added host frame validation and access helpers on `FrameLease`, including layout validation,
  host-readable/writable checks, export/materialization capability checks, and frame alias
  detection.
- Added visible-row frame access APIs for stride-aware CV processing, including immutable and
  mutable row views, all-plane visible views, contiguous visible-plane fast paths, visible payload
  byte accounting, tight-packing detection, and explicit visible-plane copy helpers.
- Added host-owned frame allocation helpers for known layouts, same-layout allocation with custom
  timestamps, allocation with stride and plane-size alignment controls, and layout-preserving
  output allocation.
- Added plane shape metadata and multi-plane validation for packed, Bayer, NV12/NV21, and
  I420/YU12/YV12 frame layouts, including overlap detection for shared-address-space backings.
- Added export capability reporting on external frame backings so memfd/dmabuf-style backings can
  advertise zero-copy export support without treating every external backing as exportable.

- Added `FrameRequirements::output_resolution`: the planner prefers the smallest mode covering
  it, and MJPEG decoded with turbojpeg (luma or RGB, `TurbojpegDecoder::with_scale`,
  `LumaDecodeScale::covering`) is decoded straight to ½, ¼ or ⅛ size, split across cores when
  frames carry restart markers. `FramePlan::output_resolution` reports the delivered size. On a
  CM5, C270 720p to 320x180: RGB 6.0 → 2.0 ms and 12.8 → 2.3 MB, luma 0.87 → 0.64 ms and
  4.0 → 2.4 MB. See `docs/frame-planning.md`.
- Added metric export: `HealthReport::metric_samples`, `HealthReport::to_prometheus` and
  `render_prometheus` (`MetricSample`, `MetricKind`, `FrameDropReason::as_str`). See
  `docs/runtime-debugging.md`.
- Added `open_recording_reader` to read recordings from any `Read`.
- Added `FrameLease::external_backing_handle` and `BufferPool::lease_sized`.
- Added VA-API encoders (`FfmpegH264Encoder::new_vaapi_nv12`, `FfmpegH265Encoder::new_vaapi_nv12`,
  registered when a VA-API driver is installed). NV12 dma-bufs (libcamera, V4L2) are imported as
  GPU surfaces without a copy; other frames are uploaded from their own memory. See
  `docs/encoding.md`.
- Added `FfmpegEncoderOptions::{low_latency, codec_options}` and `LOW_LATENCY_PRESET`.
- Added `styx_core::simd`: SIMD kernels (NEON; SSE2, SSSE3 and AVX2 chosen at run time; scalar
  oracle) for rotation/mirroring, the box pyramid, luma extraction and colour conversions, used
  by `FrameTransform`, the raw decoders, `frame_image` and the simulation backend. On a CM5 a
  720p grey rotation takes 0.22 ms instead of 6.9 ms, a mirror 0.06 ms instead of 7.2 ms.
  Cargo features `neon` and `x86` (default on). See `docs/performance.md`.
- Added a memory smoke to CI (`scripts/check-mem-smoke.sh`, `testing/perf/memory-baseline.txt`):
  heap and resident peaks per scenario, and a leak check.
- Added shared captures: `plan_many` returns a `SharedFramePlan` that runs one capture for
  several consumers, each with its own prepared frames from zero-copy shares
  (`FrameLease::into_shareable`, `FrameLease::share`). See `docs/frame-planning.md`.
- The planner scales with the Raspberry Pi ISP when an output size is requested
  (`LibcameraConfig::output_size`, `StyxConfig::libcamera_output_size`, `StepKind::Scale`),
  keeping the mode's field of view.
- Added on-demand capture: `StyxConfig::stop_when_idle` and `FramePlan::stop_when_idle` stop
  streaming while nobody pulls frames and restart on the next pull
  (`CaptureRetryStats::{idle_stops, idle_resumes}`). See `docs/reconnect.md`.
- Shared captures use both Raspberry Pi ISP outputs: consumers wanting two sizes get one each,
  scaled in hardware from the same exposure (`LibcameraConfig::second_output_size`,
  `StyxConfig::libcamera_second_output`, `CompanionKind::Scaled`).
- Consumers of a shared capture with the same requirements (apart from the region of interest)
  decode each frame once and crop their own region.
- Added `IdleStop::Pause` (`StyxConfig::pause_when_idle`, `FramePlan::pause_when_idle`): an idle
  libcamera camera stays configured and starts again in ~0.1 s instead of ~1.4 s.
- Added `styx::ipc` (Linux): `FrameServer` and `FrameClient` share frames with other processes,
  passing dma-bufs and memfds as file descriptors. See `docs/frame-server.md`.
- Added shared-capture and frame-server scenarios to the memory and perf smoke checks.
- Added `styx::ipc::CameraService`: one camera for many processes. Clients ask for frames with
  `FrameClient::request(path, &requirements)`; the service plans one shared capture for all of
  them, attaches clients that fit the running capture without a restart, refuses the ones that
  do not fit with the planner's reasons, and pauses the camera when nobody reads. Adds
  `FrameClient::{request, plan, set_roi}` and `IpcError::Rejected`; companions now travel with
  their frames. See `docs/frame-server.md` and `examples/05_apps/camera_service.rs`.
- Added `FramePlan::exportable` and `SharedFramePlan::exportable`: decoded frames go into memfd
  pools, so they reach other processes without copying.
- Added `PlannedFrames::next_frame_async` and `FrameClient::recv_async` (feature `async`).
- The planner encodes: consumers asking for H.264, H.265 or MJPEG from a camera that does not
  produce them get an encode step (hardware first), after a decode step for MJPEG cameras.
  Consumers asking for the same stream share one encoder; each starts at a keyframe, including
  after joining late or losing packets. Adds `StepKind::Encode`, `FramePlan::{encoder,
  inter_coded}`, `PlannedFrames::request_keyframe`, `FrameMeta::delta`, and
  `Codec::{request_keyframe, new_instance}`; low-latency libx264/libx265 emit IDR keyframes with
  the stream headers on request.
- `CameraService` serves several cameras (`with_cameras`, `all_cameras` with hot-plugged
  cameras; `FrameClient::{cameras, request_camera}`), checks requests before planning them,
  limits clients (`max_clients`), and can check who connects (`authorize` with
  `PeerCredentials`, `socket_mode`; `FrameServer::authorize` too).
- `FrameClient::reconnecting` keeps a client receiving across camera service restarts.
- Added the `ipc_messages` fuzz target.
- Added corruption tests for recordings, MJPEG decoders and the netcam parser, cargo-fuzz targets
  (`fuzz/`), and a public API compatibility check (`cargo-semver-checks`) in release CI.
- Development tooling for faster local and device builds (docs/development.md): `scripts/gate.sh
  --changed`, the dev loop, runs only the steps and tests of the packages a change since `dev`
  can affect (`scripts/affected.sh`: the changed packages and their dependents);
  `scripts/dev-env.sh` opts a shell into sccache and, with `STYX_BUILD_ROOT`, cargo's
  intermediate files on a faster disk; `scripts/cross-aarch64.sh` builds for the CM5 against a
  Buildroot sysroot (glibc) or statically with musl and rust-lld (no toolchain), one target
  directory per sysroot, with `--bins`, `--out` and `--tests` (a test bundle);
  `scripts/device-tests.sh` runs such a bundle on the device under the device lock (PhotonVision
  stopped and restarted by a trap, everything under `/tmp`) or here under `qemu-aarch64`;
  `[profile.device]`, release code built incrementally in many codegen units, for trying builds
  on the device; `scripts/sweep-targets.sh` lists stale target directories and deletes them in
  the background at idle disk priority (renamed aside first, btrfs subvolumes at once).

### Changed

- Integration tests build into one binary per package (`tests/it/main.rs`, one module per
  former file) instead of one per file: 53 integration-test binaries become 17 (styx 16 → 3,
  styx-algo 9 → 1, styx-softisp 5 → 1, styx-sensor 5 → 2, styx-gpuisp 4 → 1). Binaries stay
  separate only where a test needs its own process: a counting `#[global_allocator]`
  (`zero_alloc`, `no_alloc`, `descriptor_allocations`, `heap`) or process-wide state
  (styx `metrics`). The same tests run, checked by name (1017 default, 336 in the gate's
  feature set). Run one file's tests with `cargo test -p <package> --test it <module>`.
  `scripts/check-test-targets.sh` fails when a package gains a second integration-test binary
  outside its documented exceptions.

- Built and tested against Daedalus `dev` at `66659f7` (plugin ABI 9 unchanged; smaller
  `#[node]` expansions). No Styx code change; `ed7ddde` remains the minimum.

- Sensor register access is Lemnos's trait, with no Styx bus layer left (docs/portability.md
  "`RegisterBus` is Lemnos's"; Lemnos `dev` at 10269f9): `styx_sensor::RegisterBus` is
  `lemnos_hal::RegisterBus` and `styx_sensor::AsyncRegisterBus` is
  `lemnos_hal::asynch::RegisterBus`, re-exported under the old names. The sensor driver runs
  over the new `DriverBus` / `AsyncDriverBus` (a Lemnos register map plus `set_controls`, a
  kernel driver's V4L2 controls): `SensorDriver<B: DriverBus, P>`,
  `AsyncSensorDriver<B: AsyncDriverBus, P>`, and the runtime's `SensorState`, `Controls`,
  `SensorSide`, `styx-native`'s `SensorControl`/`ControlHandle` and `styx-pipeline`'s controls
  take the same bound. Migration: a custom bus implements Lemnos's `read_burst`/`write_burst`
  (overriding `read`/`write` where one value is one transfer), names its `BusError`, and adds
  `impl DriverBus for MyBus {}` (moving `set_controls` there if it has V4L2 controls);
  `I2cRegisters`/`SpiRegisters` and `MockBus` need nothing. Calls on the bus return Lemnos's
  `RegisterResult`; `BusError::from_register` converts (it now passes a `BusError` bus error
  through and keeps an `io::Error` with its errno). `styx_hal::Blocking` implements
  `lemnos_hal::asynch::RegisterBus` over a blocking map. MCU images B-D grow by 0.3-0.4 KB
  (docs/mcu.md).
- Styx's Lemnos stopgaps are Lemnos's own (Lemnos `dev` at 572e25d): `styx_hal::Blocking` is
  `lemnos_hal::asynch::Blocking` (same tuple struct; Lemnos adds embedded-hal-async `I2c`,
  `SpiDevice` and `DelayNs` and register maps over their blocking twins, Styx keeps
  `AsyncSensorPins`, `AsyncLensActuator` and `AsyncDriverBus` on it);
  `styx_hal::mock::MockI2c` is `lemnos_hal::mock::MockI2c` (re-exported with `I2cTransfer`,
  `MockOp`, `MockI2cTarget`: a bus of targets, so `MockI2c::new(addr, bits)` is
  `MockI2c::new().with_target(addr, width)`, `with_register(reg, n, value)` is
  `with_registers(addr, reg, &bytes)`, `value`/`set_dead` take the target address,
  `transactions()` is `transfers()`, `registers()` is `target(addr)` and failed transfers are
  logged too); `styx_sensor::VcmChip` is `lemnos_drivers_vcm::VcmChip` and
  `styx_sensor::VcmFormat` is `lemnos_drivers_vcm::OwnedVcmFormat` (features `alloc`,
  `serde`). Lens descriptions parse, check and compile exactly as before (TOML, postcard bytes
  and error messages: `crates/sensor/tests/lens_format.rs`). MCU images: Cortex-M0+ B-D 0.4-0.5
  KB smaller (Lemnos), Cortex-M7 B-D 0.6-0.7 KB larger (docs/mcu.md).
- `styx-native`: `SensorBus` and `SubdevBus` implement Lemnos's `RegisterBus` (bus error
  `BusError`) and `DriverBus`; `SensorBus::I2c` holds `I2cRegisters<I2cBus>`. `BridgePins` and
  `PowerSwitch` moved to `styx_native::sensor_bus`.

- The minifb preview window moved to `crates/styx/src/preview/window.rs`; its path
  (`styx::preview::PreviewWindow`, `styx::extras::preview_window`) is unchanged.

- libcamera: frames carry hops (`FrameMeta::hops`) like the native path: `Sensor` (libcamera's
  `SensorTimestamp`), `Dequeued` (the request completed), `Queued` and `Taken`, so in-process,
  frame-socket and camera-service hop tables show sensor-to-consumer latency on libcamera too.
  The capture records into its per-camera metrics (`CaptureHandle::camera_metrics`). The clock
  of `SensorTimestamp` is decided on the first frame: libcamera documents `CLOCK_BOOTTIME`, V4L2
  pipelines deliver the receiver's `CLOCK_MONOTONIC` buffer time; they differ only after a
  suspend, and the clock the timestamp is not ahead of is taken then.
- libcamera: a captured frame allocates one record (its lease) in steady state, as on the native
  path (was about 6 per frame in-process, more through IPC): request metadata is read and
  per-request controls written in place through libcamera's C API (no `ControlValue` per
  entry), each request's return slot is made once (no return record or channel message per
  frame), completed requests arrive through a preallocated queue, requeueing reuses its lists,
  and `LibcameraBacking::export_into` exports into the sender's list.
- libcamera: a buffer whose planes are on duplicates of one dma-buf (libcamera gives each plane
  its own fd) is mapped once and synced once per read (one `DMA_BUF_IOCTL_SYNC` START and END
  per frame, was one pair per plane).
- `hop_breakdown`: the frame-socket consumer uses `fetch_next` (each frame once), prints the
  server's sends, distinct frames and repeated sends, and allocations per frame are reported as
  the median one-second window (steady state) besides the whole-run average (which includes
  statistics requests and clients joining or leaving).
- `libcamera_format_check`: formats the probe did not list are skipped; `--force` requests them
  anyway.

- `PackedChannelOrder::Xrgb` / `Xbgr` are now `Bgrx` / `Rgbx` (bytes in memory order, like
  `Rgba` / `Bgra`), and `Channel` has a `Padding` variant for the unused byte of `XR24` / `XB24`.
- Rust 1.99.0: the pinned toolchain (`rust-toolchain.toml`), the MSRV (`rust-version = "1.99"`,
  also for gst-styx and pipewire-styx) and CI. MCU images are 0.1-1.6% smaller (docs/mcu.md).
- Every dependency at its newest release. Majors that reach Styx's public API, with migration:
  - `utoipa` 5 → 6 (feature `schema`): Styx's types derive utoipa 6's `ToSchema`. An app
    building an OpenAPI document from them moves to `utoipa = "6"` (upstream: `OpenApi::to_yaml`
    now returns `yaml_serde::Error`; expression `ignore` in `ToSchema` was removed).
  - `minifb` 0.28 → 0.29 (feature `preview-window`): `PreviewWindow::new`, `for_mode` and
    `for_frame` return minifb 0.29's `Error`. An app matching on it moves to `minifb = "0.29"`.
  - `bevy` 0.18 → 0.19 (feature `simulation-bevy`): internal to the simulation backend, but
    apps that build a Bevy app next to it move to 0.19. Glb scenes load through Bevy's
    `bevy_world_serialization` (`WorldAssetRoot`, formerly `SceneRoot`) instead of
    `bevy_scene`; the readback copy runs in Bevy's `RenderGraph` schedule.
- The Bevy simulation camera works again (it panicked on dev before the upgrade: glTF scenes
  spawn only reflection-registered types, now `reflect_auto_register`). Fixed while checking
  every output mode: the depth overlay is on its own render layer (the colour camera rendered
  it instead of the scene), the overlay material uses `#{MATERIAL_BIND_GROUP}`, a `Vec4`
  uniform and no prepass of its own (normals and depth showed the quad's), and depth is
  linearised for Bevy's reverse-Z projection (2.5 m to the test box, was a constant).
  - Internal only: `ffmpeg-sys-next` 8 → 9 (still generated from the system's FFmpeg headers,
    3.x to 9.x; FFmpeg is still loaded at run time by the major it was built against),
    `spin` 0.10 → 0.12, `signal-hook` 0.3 → 0.4 (tools).
- Daedalus `dev` at b6be6d4 (plugin ABI 9). `StyxFramesPlugin` registers styx-core's build
  (`#[plugin(.., crate_build)]`; styx-core gained a `build.rs` exporting its features with the
  `daedalus` feature), so a separately built dynamic plugin refused for `styx:framelease` is
  told which styx-core features differ (docs/daedalus.md). The plugin id and the type keys are
  unchanged.
- The frame socket, the camera service and its clients reuse their message buffers, frame
  records and descriptor lists, and camera service clients read memfd frames through their
  mapping cache too (they were mapped per frame): the steady-state path to a consumer in
  another process allocates only each received frame's release record, and none of it copies
  (tests with a counting allocator: `crates/styx/tests/zero_alloc.rs`, and the in-process PiSP
  path in `native_isp::pisp_lease`). The PiSP worker takes its buffers back through a fixed
  ring instead of a channel. The virtual camera's capture thread is named `styx-virtual`.
- Lighter `styx` dependency tree (default features: 65 crates to 26; `native` 86 to 64;
  `native,v4l2,uvc,async,hotplug,gpu-isp,codec-turbojpeg,raw-decoders` 116 to 88).
  **Breaking, with migration:**
  - `replay-mcap` is no longer a default feature (the MCAP reader brings binrw, enumset,
    darling and their proc macros). Builds that record or replay MCAP files ask for it:
    `styx = { version = "2.0.0", features = ["replay-mcap"] }`. Without a recording format,
    `StreamRecorder::create` and `open_recording` return `ReplayError::NoFormat`.
  - Diagnostics through `tracing` are the new `tracing` feature of `styx` (default on), of
    `styx-v4l2` and of `styx-uvc` (off; `styx/tracing` turns them on). Builds with
    `default-features = false` that want Styx's log events add `"tracing"`; without it the
    log and span macros compile to nothing. `tracing` is built without default features
    (no `tracing-attributes`).
  - `styx` no longer depends on `styx-runtime` (and through it `styx-sensor`, `styx-hal`,
    Lemnos and `toml`) outside `native`: the metrics counters come from `styx_core::metrics`,
    as they did through `styx-runtime`'s re-export.
  - `hotplug`: `LinuxVideoFsWatcher` listens to kernel uevents (`video4linux`, `media` and
    `usb` devices, through `lemnos_linux::uevent`) instead of inotify on `/dev` and sysfs;
    where no netlink socket can be opened it compares a listing of `/dev` and
    `/sys/bus/usb/devices` on every poll. Its events carry the watcher name
    `linux.video.uevent` (was `linux.video.fs`) and `/dev/...` or `/sys/devices/...` paths;
    `LinuxVideoFsWatcher::uses_uevents` is new. The `inotify` dependency is gone.
  - `styx-codec` no longer depends on `rayon`: the CPU converters (`raw-decoders`, `image`)
    split rows over a persistent pool of up to 8 threads of their own (`styx-codec-N`),
    started on first use; a conversion that finds the pool busy runs on its own thread.
    Throughput on a loaded 24-thread x86 host: Mono8 to RGB 1080p 0.32-0.54 ms to
    0.10-0.13 ms, BGRA to RGB 1080p 0.86-1.24 ms to 0.33-0.45 ms; MJPEG decode (which never
    used rayon) unchanged.
  - `futures-util` is built without default features (no `futures-macro`).
- `styx-pipeline` and `styx-softisp` use `styx_core::math::Float` instead of their own copies
  (and no longer depend on `libm` directly).
- `FrameRequest::roi` no longer claims a crop it does not make: on routes that crop only luma
  frames, an NV12 or RGB request's region is reported as `Unmet::Roi` (and refused when strict).
- `FrameLease::can_read_planes`, `has_host_readable_bytes` and `require_host_readable` follow
  `cpu_access()`: a mapped dma-buf (e.g. a native camera's cached ISP output) is readable.
- In-process plans keep the ISP's pyramid levels: the pipeline hands companions to the planner's
  stage instead of setting them aside, which made it compute them again on the CPU.
- The `styx::ipc` wire format is version 6 (frames carry their CPU access).
- The `styx::ipc` wire format is version 5 (requests carry `strict`, accepts carry `Delivered`);
  `FrameClient` connections are non-blocking.
- `FrameLease::descriptor` no longer allocates: `FrameLeaseDescriptor::planes` is a
  `SmallVec<[FramePlaneDescriptor; 4]>` (it was a `Vec`, one allocation per call).
- `FrameClient::plan` returns `Option<String>`; the `styx::ipc` wire format is version 3.
- Shared plans rank modes by the frames consumers get (as single plans do), and the Raspberry Pi
  ISP also scales for routes that decode uncompressed frames (e.g. YUYV to luma).
- Decoded planned frames keep their timestamp clock and capture instant.
- The `styx::ipc` wire format is version 2.
- `CompanionKind` has a `Scaled` variant; `.styxrec` recordings keep it, MCAP recordings keep
  pyramid levels only.
- `plan_many` now plans a shared capture; `PlanError::MultipleConsumersUnsupported` is replaced
  by `PlanError::NoConsumers`. `PlannedFrames::pipeline()` returns an `Option` (none for a
  shared consumer).
- With an output size, the planner ranks modes by the size of the frames they deliver after
  ISP or decoder scaling.
- FFmpeg encoders are low latency by default: libx264/libx265 use `tune=zerolatency` and the
  `superfast` preset. On a CM5 at 720p the libx264 defaults held 40 frames of lookahead, used
  140 MB and 61 ms per frame; now the first packet comes with the first frame, in 20 MB and
  14.5 ms.
- FFmpeg encoders read input frames in place when no conversion is needed (camera buffers are
  held until FFmpeg releases them) instead of copying each frame into an encoder frame, and
  allocate that staging frame only when a copy is needed.
- The MCAP reader reads records itself instead of through the `mcap` crate's reader, streams one
  record at a time with a 64 KiB buffer (was 1 MiB), and recovers every frame before a cut in a
  recording cut short mid-chunk.
- MJPEG decoders size output buffers to the frame (`lease_sized`) instead of their pool's chunk:
  a 320x180 luma decode no longer allocates 1 MB.
- The planner builds its JPEG luma decoder with the planned alignment, threads and scale from
  the first frame (they were only applied after an ROI change).
- FFmpeg hardware devices (VA-API) try each render node when the default fails, for machines with
  several GPUs.
- FFmpeg is no longer linked. `styx-codec` calls it through libraries loaded with `dlopen` on
  first use (by the major version it was built against), keeping `ffmpeg-sys-next` for types
  only, so `codec-ffmpeg` builds that never decode or encode through FFmpeg do not map it (CM5:
  2.2 MB of system memory per process). Registries register FFmpeg codecs without loading it;
  hardware decoders are probed only when their device exists (V4L2 M2M/request formats,
  Rockchip MPP, Jetson, NVIDIA, VA-API drivers, Intel GPUs) and on the first lookup of their
  input format. `ffmpeg-next` is no longer a dependency; `netcam-video` and
  `file-backend-video` enable `styx-codec/codec-ffmpeg-format`. A codec registry no longer
  fails to build when FFmpeg is missing at runtime; FFmpeg codecs report it when used.
- The libcamera manager now stops whenever nothing needs it (`stop_when_idle`, default on):
  after a probe and when the last capture or hotplug subscription ends. A running manager keeps
  an IPA process per camera it found; on a CM5 that is 7.6 MB PSS (12.4 MB RSS) per Raspberry
  Pi camera, whether capturing or not. Starting a capture recreates it (first frame 101 ms).
- `styx_libcamera::subscribe_hotplug_events` returns a `HotplugSubscription` with the receiver;
  the manager stays running while it is alive. `LinuxVideoFsWatcher` reports libcamera device
  changes without keeping the manager running.
- `BackendKind` and `BackendHandle` have a `Replay` variant; exhaustive matches need a new arm.
- `CompanionKind` is no longer `#[non_exhaustive]`.
- `FrameLease::visible_rows` (and so `to_visible_vec`) reads dma-buf frames whose backing maps
  their planes, as `luma_rows` already did.
- `CaptureRetryStats::netcam_retry_count` and `CaptureRetryMetrics::record_netcam_retry` are
  now `reconnect_attempts` and `record_reconnect_attempt`, shared by netcam, libcamera and V4L2.
- A V4L2 device that disappears (`ENODEV`) ends its capture with `CaptureError::Disconnected`
  instead of retrying the dequeue forever.
- `BoundedTx` is `Clone` for any element type.
- Capture queues now drop their oldest frame when full (`QueueOverflow::DropOldest`) instead of
  blocking the worker and then dropping the new frame; `Backpressure` restores the old
  behaviour.
- libcamera and V4L2 allocate `queue_depth + extra_buffers` (default 2) device buffers. libcamera
  used exactly `queue_depth`, so a full queue left it without requests and it delivered frames
  hundreds of milliseconds old. On a CM5 with a consumer taking 100 ms per frame, frame age
  drops from 674 ms to 74 ms with the defaults and to 41 ms with `latest_frame_only()`.
- The planner's `Priority::Latency` uses a one-frame queue (newest frame only).
- FFmpeg is linked without ffmpeg-next's default `device`, `filter` and `software-resampling`
  features, so `codec-ffmpeg` builds no longer load libavdevice, libavfilter or libswresample.
- V4L2 captures may use 3 buffers (was at least 4), so `latest_frame_only()` saves one buffer
  (0.6–0.7 MB for a C270).
- The default capture queue depth is 2 (was 4). With drop-oldest, deeper queues only add
  latency and device buffers.
- Processing stages keep the input frame's `FrameMeta::clock` when they keep its timestamp
  (decoded MJPEG frames previously lost it).
- libcamera frame timestamps are now the sensor's start-of-exposure time (`SensorTimestamp`)
  instead of the buffer completion time, matching V4L2 (8.2 ms vs 0.05 ms old on arrival on a
  CM5). Pyramid companions share it.
- Builds and CI do less work for the same checks (docs/development.md "The Gate"): the gate from
  a clean `target/` takes 10-20% less CPU and writes 39% less (21.2 to 12.9 GB), after an
  edit to `styx` 37% less CPU and 53% less written. `scripts/gate.sh` runs the manual gate with
  `styx` compiled for three feature sets instead of five, nextest for the tests (doctests through
  `cargo test --doc`) and `zero_alloc` once; `--full` adds the repeat stress run, every release
  feature set, the all-feature clippy, the memory smoke and libtest; steps whose tools are missing
  are skipped with a note. `[profile.dev]`: line tables only for the workspace, no debug info for
  dependencies, and `opt-level = 1` for `styx-algo`, `styx-tune`, `styx-softisp` and
  `styx-gpuisp` (the workspace's test run from about 190 to 70 CPU-seconds). Binaries without
  tests are `test = false` and feature-gated integration tests name their `required-features`, so
  `cargo test` builds no empty test binaries; `scripts/check-test-targets.sh` keeps the marking
  honest. The perf smoke makes three release builds instead of five (the memory smoke shares
  one); the file-size lint reads the sources once. CI drops the all-feature `cargo check` (the
  all-feature clippy covers it) and Release Readiness's workspace check, runs fmt and clippy in
  Release Readiness only on pushes (the CI workflow does them on pull requests), builds `styx`'s
  recording, netcam-parser and planner tests once instead of three times, and formats with
  `cargo fmt --all`. The gate now also runs `styx`'s `camera_service`, `scaled_planning`,
  `shared_planning` and `preview` tests, which no gate or CI job ran.
- Faster checks and builds (docs/development.md "Faster local builds"): `check-nostd.sh` lints
  the `no_std` crates in seven groups, one `cargo clippy` for all targets each, instead of one
  run per crate, feature set and target (85 runs), and `--verify` (CI) proves that no crate
  gets other features in its group than alone. `scripts/gate.sh --changed` runs only the steps
  and tests a change since `dev` can affect. `[profile.device]` (release code generation,
  incremental, 256 codegen units) is the default profile of `scripts/cross-aarch64.sh`; the
  `release` profile is unchanged. CI cancels superseded runs of a pull request, skips
  documentation-only pushes and pull requests (`paths-ignore`), and Release Readiness caches
  Rust artifacts. Local build directories: `.gitignore` ignores the `target-*/` directories.

### Deprecated

- `styx_sensor::Registers<R>` (any Lemnos register map is a `DriverBus` with an empty impl) and
  `styx_native::regbus` (re-exports `BridgePins`, `PowerSwitch`, `MAX_BURST`, and
  `I2cRegisterBus` = `styx_sensor::I2cRegisters<I2cBus>`).
- `VcmChip::lemnos` and `VcmFormat::with_lemnos` are the deprecated traits
  `styx_sensor::lens::{VcmChipLemnos, VcmFormatWithLemnos}` (the types are Lemnos's: use the
  chip directly and `VcmFormat::with_format`).

### Removed

- `styx-native`'s `regbus.rs` and `native-spike`'s copy of it (the I²C register bus is
  `I2cRegisters<lemnos_linux::hal::I2cBus>`; the spike takes `BridgePins` from `styx-native`),
  and Styx's own `RegisterBus`/`AsyncRegisterBus` traits with their adapters (now Lemnos's).
- `styx_hal::mock::{I2cMessage, MockI2cError}` (Lemnos's mock records `MockOp`s in
  `I2cTransfer`s and fails with `lemnos_hal::ErrorKind`).

- Removed `styx::graph` and the `daedalus-plugin` / `graph-pipeline` features (the graph-backed
  `MediaPipeline` runtime, Styx's own Daedalus nodes, `PipelineExecutionMode::Graph`, the
  `graph_*` builder options, `submit_control_event`, `graph_telemetry*`, `StyxServiceEvent::Control`,
  `GraphTelemetryStats`, `HealthReport::graph`, `RuntimeMemoryReport::graph`, and the graph drop
  reasons; `runtime_memory_report_with_styx` drops its graph argument), and the `graph_fanout` and
  `camera_graph_metrics` examples. Styx has one Daedalus path, the `daedalus` feature: build the
  graph with Daedalus and feed it frames.

### Fixed

- Planner: grey (`Frames::gray()`) from a Raspberry Pi camera behind libcamera is the Y plane of
  the ISP's processed output (a zero-copy luma view of NV12/YUV420), never the raw sensor
  stream. libcamera offers R8/GREY there only on its raw role, for sensors its camera helper
  calls mono; the OV9782 is aliased to the mono OV9281, so `Frames::gray()` picked `libcamera
  R8 1280x800`, the Bayer mosaic. Such modes are now rejected for grey requests ("the raw
  sensor stream") when the camera has a processed YUV mode, and planned only when asked for by
  format (`Frames::formats([FourCc::R8])`); their capture step says "raw sensor stream, not
  processed by the ISP". Cameras without an ISP (mono USB, virtual, replays) still deliver
  their GREY/R8 frames as they are. `FramePlan::raw_sensor_stream()` says whether a plan's
  frames are a raw sensor stream.
- `styx-record` records the Y plane of the ISP's output in direct mode (it recorded raw R8 on
  the CM5), and no longer makes the camera drop frames: it read the camera's controls on its
  frame loop every second, and a libcamera control read waits for the capture thread (up to a
  frame period per control), so 24 controls held the loop for ~0.4 s and overflowed the
  8-frame queue (36 frames lost in 9 gaps in 10 s on the CM5). Controls are now read on their
  own thread (`source::ControlReader`, `record::ControlPoller`); the loop only copies each
  frame's Y plane into a writer buffer and releases the frame at once.

- Camera service frames from libcamera and replayed captures carry their sequence number
  (`FrameMeta::sequence()` on the client): only captures that stamp their hops when they
  deliver a frame (virtual, V4L2, native, UVC) sent it; the service now fills the hops trailer
  from the frame's backend metadata. No protocol change.

- libcamera: requesting a format libcamera does not offer no longer aborts the process. Styx
  set any requested pixel format on the stream and let `validate()` adjust it, and PiSP asserts
  (`toPiSPImageFormat`, "Pixel format <INVALID> unsupported") on formats it has no V4L2 mapping
  for, e.g. `RGBA` (DRM `AB24`): `libcamera_format_check` requested it after reporting it "not
  among the probed modes", because neither it nor the backend checked. Every stream (all
  pipelines) is now set only to a pixel format from libcamera's `StreamFormats` for it (the
  requested role, else another role that offers it; raw requests get the offered format with
  its packing modifier); otherwise the capture fails with `CaptureError::InvalidConfig`
  ("libcamera does not offer RGBA on this camera (offers ...)") without asking libcamera.
  `RGBA` and `BGRA` are also refused up front on PiSP sensors, as `InvalidConfig` (was
  `Backend`, which is retried).

- A control request right after the camera service restarted failed with a broken pipe on the
  kept control connection; it is now sent again on a fresh connection, like a reset one.
- `XR24` and `XB24` byte order now follows V4L2 and DRM everywhere: `XR24` (V4L2 `XBGR32`, DRM
  `XRGB8888`, what libcamera delivers) is bytes B, G, R, x; `XB24` (V4L2 `RGBX32`, DRM
  `XBGR8888`) is R, G, B, x. Colour output changes for `XR24` / `XB24` users, whose red and
  blue were swapped:
  - styx-codec's `xr24-strip` decoder read `XR24` as R, G, B (now B, G, R), and `xb24-strip`
    read `XB24` as B, G, R (now R, G, B);
  - `frame_to_dynamic_image` (the `image` bridge) had the same swap for both;
  - `FourCc::layout_info` gave `XR24` as x, R, G, B and `XB24` as x, B, G, R (now `Bgrx` /
    `Rgbx`, checked against `format::drm` in a test).
- The libcamera backend and probe name libcamera's formats by memory layout
  (`styx_core::format::drm`): libcamera uses DRM fourccs, so its `RG24` (`RGB888`, bytes B, G,
  R) is now Styx `BG24` and its `BG24` Styx `RG24` (before: the other way round, swapped
  colours), its `AB24` / `AR24` are `RGBA` / `BGRA`, and CSI-2 packed raw is `pBAA` and friends
  by modifier. Requests go the other way: `XR24` / `XB24` are asked for as themselves (before:
  the unknown codes `RGB0` / `BGR0`, which libcamera replaced), `RG24` / `BG24` as DRM `BG24` /
  `RG24` (before: `RGB3` / `BGR3`), `RGBA` / `BGRA` as `AB24` / `AR24`.

- The native software ISP no longer runs on every core by default: on the dev box's CM5 at
  2.4 GHz, four ISP threads (or three at 120 fps) hung the board within seconds (no kernel
  message, watchdog reboot), camera or not, which is what `STYX_NATIVE_ISP=software` captures
  and stills did. The default is now half the cores, at most 4 (`NativeIspConfig::soft_threads`;
  `docs/native-stack/pipeline.md`, "All four cores"). Capture buffers smaller than the format's
  `sizeimage` (driver buffers, or imported dma-bufs at the size the kernel allocated,
  `styx_kernel::dma_heap::dmabuf_size`) are refused before they are queued.
- Damaged input no longer crashes Styx or makes it allocate far beyond the data:
  - `.styxrec` payload lengths allocated up to 4 GiB before reading (118 MB from an 8 KB file)
    and companion frames could nest without bound;
  - MCAP: a damaged chunk header made the reader buffer up to 4 GiB, record lengths inside a
    chunk underflowed (a panic in debug builds), a string length could reserve 64 MiB, a repeated
    pyramid level panicked, and incomplete frames were kept without bound;
  - `FrameLease::from_visible_bytes` allocated the full frame for a recorded resolution before
    checking the payload size;
  - MJPEG decoders sized their output from the JPEG header: a bit error claiming 32767x32767
    zero-filled gigabytes. Frames claiming more than 4x the stream's pixels are now refused;
  - netcam multipart header lines were read without a length limit.
- Probing libcamera failed while any libcamera camera was capturing ("manager mutation blocked
  by active camera use"), which also broke reconnects and planning next to a running camera.
  Probes now use shared access and return complete descriptors during capture.
- A restarted libcamera manager returned from `start()` before registering its cameras, so
  probes after an idle stop found no cameras. Stopped managers are now dropped and recreated.
- Exited libcamera IPA helper processes are reaped when the manager stops instead of staying as
  zombies.
- Whole-frame copies of ROI crop views failed plane-length validation: the last row of a view
  ends after its visible bytes, not a full stride.
- The runtime memory report read kernel dma-buf sizes (`/sys/kernel/debug/dma_buf/bufinfo`,
  zero-padded decimal) as hex, overstating them about 13x (270 MiB reported for 20.6 MiB).
- The libcamera manager is now stopped at process exit when no camera is in use, so processes
  no longer leave orphaned `raspberrypi_ipa_proxy` helpers behind.
- Worked around a libcamera-rs 0.7.0 plane fd leak in `OwnedFrameBuffer::new` in a way that stays
  correct once the upstream fix is released.
- Fixed pre-existing CI failures: `handle_tests` missing import under `libcamera`, clippy lints in
  netcam, `frame_sizing`, `memory`, aarch64-only raw decoders and `recording` tests, example
  formatting, and the file-size baseline.
- Fixed turbojpeg MJPEG decoding rejecting frames with recoverable libjpeg warnings (e.g.
  "extraneous bytes before marker"), which common UVC cameras such as the Logitech C270 emit;
  `TurbojpegDecoder` and `TurbojpegLumaDecoder` now accept them as libjpeg does.
- Fixed V4L2 GREY/R8 capture dropping every frame (stride was assumed to be 3 bytes per pixel);
  GREY/R8 buffers are now also delivered zero-copy.
- The runtime RG24 fallback decoder now prefers libturbojpeg over `jpeg-decoder` for MJPEG when
  both are enabled, and GREY conversion of MJPEG uses the luma-only turbojpeg path.

## [2.0.0] - 2026-05-01

### Added

- Added a release-oriented multi-crate workspace versioned as `2.0.0` across `styx`,
  `styx-core-rs`, `styx-capture`, `styx-codec`, `styx-libcamera`, `styx-v4l2`, and
  `styx-examples`.
- Added runtime capture configuration through `StyxConfig`, including queue depth, buffer pool
  sizing, capture enqueue timeouts, V4L2 worker timing, libcamera startup/control/idle timing,
  netcam HTTP/backoff timing, file image cache limits, and transform pool sizing.
- Added focused public config/source types for the v2 API surface, including `CaptureConfig`,
  `BackendConfig`, `V4l2Config`, `LibcameraConfig`, `NetcamConfig`, `FileBackendConfig`,
  `TransformConfig`, `VirtualSourceConfig`, `VirtualCaptureConfig`, `NetcamSourceConfig`,
  `FileSourceConfig`, `GraphPolicy`, and `SinkPolicy`.
- Added typed capture startup policy support with resilient retries, transient libcamera retry
  handling, optional control dropping on rejected controls, and optional TDN fallback behavior.
- Added async capture helpers for Tokio users, including async receive/control methods,
  async startup retry sleeps, async netcam workers, and blocking-worker helpers for CPU-heavy
  pipeline stages.
- Added netcam MJPEG and optional FFmpeg-backed video ingestion with configurable request,
  connect, read, retry, stop-poll, and queue-send behavior.
- Added file replay support for image and optional video sources, including playback controls
  and decoded image cache tuning.
- Added simulation capture support behind `simulation-bevy`, including runtime state,
  visualization/readback plumbing, and depth/RGB output handling.
- Added graph-pipeline support through the Daedalus plugin integration, including frame nodes,
  source/sink nodes, codec nodes, runtime nodes, control event routing, fanout policies, and
  graph telemetry reporting.
- Added richer observability with `tracing` spans/events, stage metrics, health reports, queue
  telemetry, drop reasons, residency transitions, external backing tracking, memory statistics,
  and recent stage/control error reporting.
- Added Linux zero-copy oriented frame residency and backing support, including shared memfd
  pools, export/import helpers, residency capability reporting, and shared decode/encode paths
  where codecs support them.
- Added typed codec policy and selector improvements, including `CodecImplementationId`,
  typed preferred lookup/process APIs, codec implementation priority, hardware bias controls,
  codec family selectors, and registry sizing configuration.
- Added raw decoder coverage for common packed, planar, semi-planar, Bayer, and mono formats,
  including NEON-accelerated paths where available and feature-gated raw decoder registration.
- Added runtime-configurable transform and dynamic image staging pools, with pool telemetry for
  memory and allocation debugging.
- Added pipeline memory telemetry for Linux shared decode and encode pools so high-resolution
  zero-copy pool sizing and retained capacity are visible at runtime.
- Added capture retry and shutdown telemetry for startup retries, netcam reconnects, async drop
  detaches, worker joins, drains, and teardown latency.
- Added typed service event kind accessors for service, pipeline worker, sink lifecycle, and
  recording lifecycle events so consumers can filter event streams without matching strings.
- Added hotplug/watch runtime support with sync and async subscription paths.
- Added release validation assets and scripts, including feature-combination checks, perf smoke
  checks, file-size checks, Docker facade tests, and organized example binaries.

### Changed

- Bumped the public workspace release from `1.0.0` to `2.0.0` and updated crate README install
  snippets to match.
- Reworked the public capture API around `CaptureRequest`, `CaptureHandle`, `CaptureSource`,
  typed modes/descriptors/controls, and request-local runtime configuration.
- Changed virtual, netcam, and file replay examples to use request-based source builders:
  `CaptureRequest::virtual_source`, `CaptureRequest::netcam_source`, and
  `CaptureRequest::file_source`.
- Changed simulation support to live under the explicit `styx::simulation` feature module instead
  of the core capture API surface; simulation examples now import simulation APIs directly.
- Split capture, codec, core buffer/format/queue, V4L2 probing, and libcamera probing concerns
  into dedicated crates while keeping `styx` as the high-level facade crate.
- Reorganized examples into top-level workflow groups for quickstart, capture, graph, codecs,
  performance, and app-style examples.
- Tightened feature gating so heavy stacks remain opt-in: FFmpeg, netcam, file backend,
  libcamera, V4L2, preview windows, Bevy simulation, hotplug, graph pipeline, serde, and schema
  support are all controlled through explicit features.
- Changed pipeline processing to preserve stage error details through result-returning methods
  while keeping infallible iterator-style convenience methods for simple callers.
- Changed async netcam MJPEG enqueue behavior to use async queue waiting with a timeout instead
  of blocking Tokio runtime workers on synchronous queue sends.
- Changed bounded queue close/send synchronization so queue closure has a clearer ordering
  against concurrent send attempts.
- Changed pipeline teardown so stopping one pipeline no longer resets process-wide transform pool
  configuration that may be used by another live pipeline.
- Changed capture backend pool sizing to use a typed `PoolLimits` value instead of passing raw
  `(min, bytes, spare)` tuples through backend code.
- Changed codec registry matching to normalize implementation IDs consistently and added typed
  APIs for callers that want to avoid stringly typed implementation preferences.
- Changed runtime codec selector boundaries to compare normalized `CodecImplementationId` values
  instead of raw implementation strings.
- Changed raw decoder implementations to share descriptor construction, owned output allocation,
  and Linux shared-output boilerplate.
- Changed codec residency detection to use `FourCc` helpers and descriptor methods instead of
  repeated ad hoc compressed-format checks.
- Changed observability to record more runtime data, including async queue waits/wakes,
  per-stage p50/p95 timing, drop causes, graph drops/latest replacements, sink lifecycle events,
  and recorder indexing events.
- Changed netcam MJPEG parsing so sync and async workers share header, content-length, boundary,
  and oversized-frame parser state while keeping their IO paths separate.
- Changed libcamera manager lifecycle handling to use typed runtime configuration, probe cache
  TTL controls, active camera use guards, and optional idle-stop behavior.
- Changed graph policy helpers from workflow names to behavior names: `latest_only`,
  `bounded_blocking`, and `bounded_drop_oldest`.
- Changed pipeline recording wiring from `record_output(recorder)` to the generic
  `.sink("recording", recorder)` builder shape.
- Changed codec family metadata and helpers to describe output/capability behavior instead of
  preview/recording application roles.

### Removed

- Removed the old crate-local `crates/styx/examples` layout in favor of the workspace-level
  `examples` crate and grouped example directories.
- Removed unconditional process-wide cleanup of transform and image staging pools from pipeline
  teardown; callers can still explicitly reset configurable global pools when they intentionally
  want to clear them.
- Removed several ad hoc magic tuples and backend-local pool sizing conventions in favor of typed
  configuration and centralized tunables.
- Removed repeated string matching for codec implementation preferences where typed
  `CodecImplementationId` APIs can now be used.
- Removed workflow-specific config presets from the core API: `StyxConfig::low_latency_preview`,
  `StyxConfig::reliable_recording`, and `StyxConfig::netcam_preview`.
- Removed workflow-specific preview/analysis/recorder graph sink helpers from public exports in
  favor of generic frame sink registration.
- Removed virtual/netcam/file demo constructors from the main prelude; use the request-based
  source builders for application code or import lower-level constructors from `capture_api` when
  needed.
- Removed simulation assets from the facade crate and moved them under the examples crate.
- Removed legacy compatibility assumptions around backend startup and feature availability:
  disabled optional backends now report typed `BackendMissing` errors instead of being hidden
  behind implicit fallback behavior.

### Fixed

- Fixed async netcam backpressure so full output queues no longer block Tokio core runtime
  workers during MJPEG frame enqueue.
- Fixed a close/send race in the bounded queue by synchronizing close with the same wait-state
  lock used by send paths.
- Fixed cross-pipeline transform pool interference caused by resetting process-wide pool state
  during unrelated pipeline teardown.
- Fixed release-readiness issues around pool sizing readability and typed codec preference
  ergonomics without breaking existing string-based APIs.
- Fixed async drop behavior so dropping a capture handle from a Tokio runtime does not block on
  worker joins.
- Fixed blocking pipeline worker failures so terminal decode/encode/graph errors are returned and
  emitted through service lifecycle events.
- Fixed netcam reconnect behavior so ended or failing MJPEG streams back off instead of reconnecting
  in a tight loop.

## [1.0.0] - 2026-04-19

- Standardized the workspace layout, docs, CI, linting, and helper scripts.
- Centralized workspace dependencies and brought all crate members under the shared root configuration.
- Added Docker-backed facade validation for the virtual-camera flow.
- Added `scripts/check-file-sizes.sh`, `scripts/ci.sh`, and `scripts/repo-clean.sh`.
