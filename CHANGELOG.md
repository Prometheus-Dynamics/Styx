# Changelog

All notable changes to this workspace should be documented in this file.

The format is based on Keep a Changelog and this project follows Semantic Versioning.

## [Unreleased]

### Added

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
- Added corruption tests for recordings, MJPEG decoders and the netcam parser, cargo-fuzz targets
  (`fuzz/`), and a public API compatibility check (`cargo-semver-checks`) in release CI.

### Changed

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

### Fixed

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
